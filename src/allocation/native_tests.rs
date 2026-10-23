use super::native::azure_history_has_unique_incarnation;
use super::unix_now;
use super::{
    AllocationAdmission, ChildAttemptRecord, Journal, PlacementIdentity, PoolSnapshot, SystemTime,
    UNIX_EPOCH,
};

fn azure_offer_json() -> String {
    serde_json::json!({
        "candidate": {
            "identity": "azure:subscription-a:runner-a",
            "os": "linux",
            "architecture": "x64",
            "image": "image-a",
            "vcpu": 2,
            "memory_gib": 4,
            "machine_type": "Standard_D2s_v5",
            "machine_family": null,
            "cloud": "azure",
            "purchasing": "spot",
            "account": "subscription-a",
            "location": "eastus",
            "gpu_vendor": null,
            "gpu_model": null,
            "gpu_count": 0,
            "gpu_memory_gib": null,
            "boot_disk_class": "Premium_LRS",
            "boot_disk_gib": 32,
            "boot_disk_iops": 0,
            "boot_disk_throughput_mib_s": 0,
            "local_scratch_gib": 0,
            "capabilities": [],
            "price": null,
            "interruption_rate_per_hour": null,
            "allocation_failure_rate": null,
            "mean_create_seconds": null,
            "observation_age_seconds": null,
            "observation_window": null,
            "observation_samples": 0,
            "definitive_create_outcomes": 0,
            "interruption_samples": 0,
            "native_capacity_hint": null
        },
        "selection": {
            "binding_id": "azure-child",
            "source_revision": "revision-a",
            "binary_sha256": "a",
            "configuration_sha256": "b",
            "flavor": "Standard_D2s_v5",
            "image": "image-a",
            "approved_extras": {}
        },
        "launch_facts": {
            "provider": "azure",
            "facts": {
                "subscription_id": "subscription-a",
                "location": "eastus",
                "image_version": "version-a",
                "os_disk_sku": "Premium_LRS",
                "public_ip_sku": null,
                "minimum_os_disk_gib": 32,
                "minimum_os_disk_source": "image",
                "image_licensing": "no_additional_software_charge",
                "image_licensing_source": "image",
                "maximum_data_disk_count": null,
                "maximum_os_disk_iops": null,
                "maximum_os_disk_throughput_mib_s": null,
                "vm_size_capabilities": {},
                "ephemeral_os_disk_supported": false
            }
        }
    })
    .to_string()
}

fn azure_identity(controller_id: &str, vm_id: &str) -> PlacementIdentity {
    PlacementIdentity {
            controller_id: controller_id.to_owned(),
            cloud: "azure".to_owned(),
            account_id: "subscription-a".to_owned(),
            location: "eastus".to_owned(),
            resource_id: "/subscriptions/subscription-a/resourceGroups/runner-a/providers/Microsoft.Compute/virtualMachines/runner-a".to_owned(),
            host_id: None,
            dependency_ids: [("vm_id".to_owned(), vm_id.to_owned())].into(),
        }
}

fn persist_azure_attempt(
    journal: &Journal,
    allocation_id: &str,
    controller_id: &str,
    offer_json: &str,
    binding_json: &str,
    identity: Option<&PlacementIdentity>,
) {
    journal
        .reserve_allocation(
            allocation_id,
            controller_id,
            "runner-a",
            "snapshot-a",
            "pool-a",
        )
        .unwrap_or_else(|_| panic!("reserve Azure allocation"));
    assert!(
        journal
            .claim_allocation_selection(allocation_id)
            .unwrap_or_else(|_| panic!("claim Azure selection"))
    );
    journal
        .set_allocation_selection(
            allocation_id,
            offer_json,
            binding_json,
            "azure:subscription-a",
            "catalog-a",
        )
        .unwrap_or_else(|_| panic!("persist Azure selection"));
    journal
        .insert_child_attempt(&ChildAttemptRecord {
            allocation_id: allocation_id.to_owned(),
            attempt_number: 1,
            state: "reserved".to_owned(),
            offer_json: offer_json.to_owned(),
            binding_json: binding_json.to_owned(),
            detail: None,
            created_at: unix_now(),
            dispatched_at: None,
            completed_at: None,
        })
        .unwrap_or_else(|_| panic!("persist Azure child attempt"));
    assert!(
        journal
            .mark_child_dispatched(
                allocation_id,
                1,
                &AllocationAdmission {
                    snapshot_id: "snapshot-a".to_owned(),
                    repository_id: 7,
                    run_id: 1,
                    run_attempt: 1,
                    now: unix_now(),
                },
            )
            .unwrap_or_else(|_| panic!("mark Azure child dispatched"))
    );
    if let Some(identity) = identity {
        assert!(
            journal
                .complete_allocation(
                    allocation_id,
                    "{}",
                    &serde_json::to_string(identity)
                        .unwrap_or_else(|_| panic!("serialize Azure identity")),
                )
                .unwrap_or_else(|_| panic!("retain Azure identity"))
        );
    }
}

#[test]
fn retained_azure_same_path_attempts_make_incarnation_ambiguous() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "outrider-azure-history-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory)
        .unwrap_or_else(|_| panic!("create Azure history journal directory"));
    let path = directory.join("journal.sqlite");
    let journal = Journal::open(&path).unwrap_or_else(|_| panic!("open Azure history journal"));
    journal
        .save_pool_snapshot(&PoolSnapshot {
            id: "snapshot-a".to_owned(),
            repository_id: 7,
            request_token: "request-a".to_owned(),
            config_json: "{}".to_owned(),
            state: "enabled".to_owned(),
            garm_pool_id: Some("pool-a".to_owned()),
            expires_at: unix_now() + 3600,
        })
        .unwrap_or_else(|_| panic!("persist Azure history snapshot"));
    let offer_json = azure_offer_json();
    let binding_json = "{}";
    let identity = azure_identity("controller-current", "vm-current");
    persist_azure_attempt(
        &journal,
        "current",
        "controller-current",
        &offer_json,
        binding_json,
        Some(&identity),
    );
    let current = journal
        .all_allocations()
        .unwrap_or_else(|_| panic!("query current Azure journal history"))
        .into_iter()
        .find(|allocation| allocation.allocation_id == "current")
        .unwrap_or_else(|| panic!("current Azure journal row"));
    assert!(azure_history_has_unique_incarnation(
        std::slice::from_ref(&current),
        &current,
        &identity,
    ));
    let replacement_identity = azure_identity("controller-current", "vm-replacement");
    assert!(!azure_history_has_unique_incarnation(
        std::slice::from_ref(&current),
        &current,
        &replacement_identity,
    ));

    persist_azure_attempt(
        &journal,
        "prior-unknown",
        "controller-prior",
        &offer_json,
        binding_json,
        None,
    );
    drop(journal);
    let journal = Journal::open(&path).unwrap_or_else(|_| panic!("reopen Azure history journal"));
    let history = journal
        .all_allocations()
        .unwrap_or_else(|_| panic!("query reopened Azure journal history"));
    let current = history
        .iter()
        .find(|allocation| allocation.allocation_id == "current")
        .unwrap_or_else(|| panic!("reopened current Azure allocation"));
    assert!(!azure_history_has_unique_incarnation(
        &history, current, &identity,
    ));
    drop(journal);

    let connection = rusqlite::Connection::open(&path)
        .unwrap_or_else(|_| panic!("open Azure history for damaged-offer fixture"));
    connection
        .execute(
            "UPDATE allocations SET selected_offer_json=NULL WHERE allocation_id='prior-unknown'",
            [],
        )
        .unwrap_or_else(|_| panic!("remove prior retained Azure offer"));
    drop(connection);
    let journal =
        Journal::open(&path).unwrap_or_else(|_| panic!("reopen damaged Azure history journal"));
    let history = journal
        .all_allocations()
        .unwrap_or_else(|_| panic!("query damaged Azure journal history"));
    let current = history
        .iter()
        .find(|allocation| allocation.allocation_id == "current")
        .unwrap_or_else(|| panic!("current Azure allocation after offer damage"));
    assert!(!azure_history_has_unique_incarnation(
        &history, current, &identity,
    ));
    drop(journal);
    std::fs::remove_dir_all(directory)
        .unwrap_or_else(|_| panic!("remove Azure history journal directory"));
}
