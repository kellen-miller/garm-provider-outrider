use super::*;

struct FailedCreationFixture {
    directory: TestDirectory,
    server: HttpFixture,
    journal: Arc<Journal>,
    settings: IngressSettings,
}

impl FailedCreationFixture {
    async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let directory = TestDirectory::new()?;
        let server = HttpFixture::start(FixtureMode::GarmPool).await?;
        server
            .state
            .created_pool
            .lock()
            .map_err(|_| io::Error::other("pool fixture poisoned"))?
            .replace(pool_response());
        server
            .state
            .pool_instances
            .lock()
            .map_err(|_| io::Error::other("instance fixture poisoned"))?
            .push(json!({
                "id":"instance-201", "name":"runner-refused", "pool_id":"pool-201",
                "provider_name":"outrider", "status":"error", "runner_status":"pending"
            }));
        let settings = ingress_settings(&directory, "http://127.0.0.1:1/", &server.origin)?;
        let journal = directory.journal()?;
        let snapshot = draining_pool_snapshot(&settings);
        journal.save_pool_snapshot(&snapshot)?;
        journal.reserve_allocation(
            "refused",
            &settings.controller_id,
            "runner-refused",
            &snapshot.id,
            "pool-201",
        )?;
        journal.tombstone_allocation(&settings.controller_id, "runner-refused")?;
        journal.set_allocation_state("refused", "deleted", None)?;
        Ok(Self {
            directory,
            server,
            journal,
            settings,
        })
    }

    async fn drain(&self) -> Result<usize, Box<dyn std::error::Error>> {
        Ok(IngressService::new(
            config_with_ingress(self.settings.clone()),
            self.journal.clone(),
        )?
        .drain_expired_pools(1)
        .await?)
    }
}

#[tokio::test]
async fn failed_unspawned_instance_is_deleted_before_its_pool() {
    let fixture = FailedCreationFixture::new().await.unwrap();
    assert_eq!(fixture.drain().await.unwrap(), 1);
    let requests = fixture.server.requests();
    let instance_delete = requests
        .iter()
        .position(|request| {
            request.method == "DELETE" && request.path == "/api/v1/instances/instance-201"
        })
        .expect("failed instance removed");
    let pool_delete = requests
        .iter()
        .position(|request| request.method == "DELETE" && request.path.ends_with("/pools/pool-201"))
        .expect("pool removed");
    assert!(instance_delete < pool_delete);
    assert_eq!(fixture.drain().await.unwrap(), 0);
}

struct AllocationGuardCase {
    name: &'static str,
    sql: &'static str,
}

#[tokio::test]
async fn failed_instance_cleanup_requires_terminal_unspawned_ownership() {
    for case in [
        AllocationGuardCase {
            name: "unknown",
            sql: "DELETE FROM allocations",
        },
        AllocationGuardCase {
            name: "controller",
            sql: "UPDATE allocations SET controller_id='other'",
        },
        AllocationGuardCase {
            name: "runner",
            sql: "UPDATE allocations SET runner_name='other'",
        },
        AllocationGuardCase {
            name: "pool",
            sql: "UPDATE allocations SET pool_id='other'",
        },
        AllocationGuardCase {
            name: "snapshot",
            sql: "UPDATE allocations SET snapshot_id='other'",
        },
        AllocationGuardCase {
            name: "no fence",
            sql: "UPDATE allocations SET tombstone=0",
        },
        AllocationGuardCase {
            name: "active",
            sql: "UPDATE allocations SET state='deleting'",
        },
        AllocationGuardCase {
            name: "dispatched",
            sql: "UPDATE allocations SET dispatched_at=1",
        },
        AllocationGuardCase {
            name: "returned",
            sql: "UPDATE allocations SET returned_at=1",
        },
        AllocationGuardCase {
            name: "provider result",
            sql: "UPDATE allocations SET provider_instance_json='{}'",
        },
        AllocationGuardCase {
            name: "native identity",
            sql: "UPDATE allocations SET native_identity_json='{}'",
        },
        AllocationGuardCase {
            name: "child intent",
            sql: "INSERT INTO child_attempts(allocation_id,attempt_number,state,offer_json,binding_json,created_at) VALUES ('refused',1,'reserved','{}','{}',1)",
        },
    ] {
        let fixture = FailedCreationFixture::new().await.unwrap();
        rusqlite::Connection::open(fixture.directory.path.join("journal.sqlite3"))
            .unwrap()
            .execute(case.sql, [])
            .unwrap();
        assert_eq!(fixture.drain().await.unwrap(), 0, "{}", case.name);
        assert!(
            fixture
                .server
                .requests()
                .iter()
                .all(|request| request.method != "DELETE"),
            "{}",
            case.name
        );
    }
}

struct GarmGuardCase {
    field: &'static str,
    value: Value,
}

#[tokio::test]
async fn failed_instance_cleanup_preserves_registered_or_changed_instances() {
    for case in [
        GarmGuardCase {
            field: "provider_name",
            value: json!("other"),
        },
        GarmGuardCase {
            field: "provider_id",
            value: json!("native-vm"),
        },
        GarmGuardCase {
            field: "agent_id",
            value: json!(99),
        },
        GarmGuardCase {
            field: "runner_status",
            value: json!("busy"),
        },
        GarmGuardCase {
            field: "status",
            value: json!("running"),
        },
        GarmGuardCase {
            field: "pool_id",
            value: json!("other"),
        },
        GarmGuardCase {
            field: "name",
            value: json!("unknown"),
        },
        GarmGuardCase {
            field: "id",
            value: json!(""),
        },
        GarmGuardCase {
            field: "job",
            value: json!({"job_id":301}),
        },
        GarmGuardCase {
            field: "addresses",
            value: json!([{"address":"192.0.2.1"}]),
        },
    ] {
        let fixture = FailedCreationFixture::new().await.unwrap();
        fixture.server.state.pool_instances.lock().unwrap()[0][case.field] = case.value;
        assert_eq!(fixture.drain().await.unwrap(), 0, "{}", case.field);
        assert!(
            fixture
                .server
                .requests()
                .iter()
                .all(|request| request.method != "DELETE"),
            "{}",
            case.field
        );
    }

    let fixture = FailedCreationFixture::new().await.unwrap();
    let mut changed = fixture.server.state.pool_instances.lock().unwrap()[0].clone();
    changed["provider_id"] = json!("newly-visible-native-id");
    fixture
        .server
        .state
        .instance_detail_override
        .lock()
        .unwrap()
        .replace(changed);
    assert_eq!(fixture.drain().await.unwrap(), 0);
    assert!(
        fixture
            .server
            .requests()
            .iter()
            .all(|request| request.method != "DELETE")
    );
}

#[tokio::test]
async fn failed_instance_delete_outage_preserves_pool_for_retry() {
    let fixture = FailedCreationFixture::new().await.unwrap();
    fixture
        .server
        .state
        .instance_delete_status
        .store(503, Ordering::SeqCst);
    assert_eq!(fixture.drain().await.unwrap(), 0);
    assert!(
        !fixture
            .server
            .requests()
            .iter()
            .any(|request| request.method == "DELETE" && request.path.ends_with("/pools/pool-201"))
    );
    fixture
        .server
        .state
        .instance_delete_status
        .store(200, Ordering::SeqCst);
    assert_eq!(fixture.drain().await.unwrap(), 1);
}

#[tokio::test]
async fn failed_instance_async_delete_waits_for_garm_absence() {
    let fixture = FailedCreationFixture::new().await.unwrap();
    fixture
        .server
        .state
        .defer_instance_delete
        .store(true, Ordering::SeqCst);
    assert_eq!(fixture.drain().await.unwrap(), 0);
    assert_eq!(fixture.drain().await.unwrap(), 0);
    let requests = fixture.server.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "DELETE"
                && request.path == "/api/v1/instances/instance-201")
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.method == "DELETE" && request.path.ends_with("/pools/pool-201"))
    );
    fixture.server.state.pool_instances.lock().unwrap().clear();
    assert_eq!(fixture.drain().await.unwrap(), 1);
}
