use super::*;

#[derive(Clone, Copy)]
pub(super) enum CloudApi {
    Azure,
    Gcp,
}

#[derive(Clone)]
pub(super) struct CloudHttpState {
    pub(super) api: CloudApi,
    pub(super) lifecycle_log: PathBuf,
    pub(super) requests: Arc<Mutex<Vec<(String, String)>>>,
    pub(super) native_reads: Arc<AtomicU64>,
    pub(super) quote_reads: Arc<AtomicU64>,
    pub(super) catalog_reads: Arc<AtomicU64>,
    pub(super) quote_api_failure: Arc<AtomicBool>,
    pub(super) quote_api_failures: Arc<AtomicU64>,
    pub(super) invalid_image_metadata: Arc<AtomicBool>,
    pub(super) invalid_image_responses: Arc<AtomicU64>,
}

pub(super) struct CloudHttpFixture {
    pub(super) endpoint: String,
    pub(super) state: CloudHttpState,
    pub(super) task: JoinHandle<io::Result<()>>,
}

impl Drop for CloudHttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
pub(super) struct SystemAzureCredential;

#[async_trait]
impl TokenCredential for SystemAzureCredential {
    async fn get_token(
        &self,
        _scopes: &[&str],
        _options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        Ok(AccessToken::new(
            "synthetic-local-token",
            OffsetDateTime::from_unix_timestamp(4_102_444_800)
                .unwrap_or_else(|error| panic!("fixture token expiry: {error}")),
        ))
    }
}

#[derive(Debug)]
pub(super) struct SystemGcpCredential;

impl CredentialsProvider for SystemGcpCredential {
    async fn headers(
        &self,
        _extensions: Extensions,
    ) -> Result<CacheableResource<HeaderMap>, google_cloud_auth::errors::CredentialsError> {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer synthetic-local-token"),
        );
        Ok(CacheableResource::New {
            entity_tag: EntityTag::new(),
            data: headers,
        })
    }

    async fn universe_domain(&self) -> Option<String> {
        None
    }
}

impl AccessTokenCredentialsProvider for SystemGcpCredential {
    async fn access_token(
        &self,
    ) -> Result<GcpAccessToken, google_cloud_auth::errors::CredentialsError> {
        Ok(GcpAccessToken {
            token: "synthetic-local-token".to_owned(),
        })
    }
}

pub(super) async fn cloud_http_fixture(
    api: CloudApi,
    lifecycle_log: PathBuf,
) -> io::Result<CloudHttpFixture> {
    let listener = TokioTcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let state = CloudHttpState {
        api,
        lifecycle_log,
        requests: Arc::new(Mutex::new(Vec::new())),
        native_reads: Arc::new(AtomicU64::new(0)),
        quote_reads: Arc::new(AtomicU64::new(0)),
        catalog_reads: Arc::new(AtomicU64::new(0)),
        quote_api_failure: Arc::new(AtomicBool::new(false)),
        quote_api_failures: Arc::new(AtomicU64::new(0)),
        invalid_image_metadata: Arc::new(AtomicBool::new(false)),
        invalid_image_responses: Arc::new(AtomicU64::new(0)),
    };
    let app = Router::new()
        .fallback(any(cloud_http_request))
        .with_state(state.clone());
    let task =
        tokio::spawn(async move { axum::serve(listener, app).await.map_err(io::Error::other) });
    Ok(CloudHttpFixture {
        endpoint,
        state,
        task,
    })
}

pub(super) async fn cloud_http_request(
    State(state): State<CloudHttpState>,
    request: Request<Body>,
) -> Response<Body> {
    let method = request.method().to_string();
    let path = request.uri().path_and_query().map_or_else(
        || request.uri().path().to_owned(),
        |value| value.as_str().to_owned(),
    );
    if let Ok(mut requests) = state.requests.lock() {
        requests.push((method.clone(), path.clone()));
    }
    if method != "GET" {
        return response(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"read-only fixture"}),
        );
    }
    let deleted = fs::read_to_string(&state.lifecycle_log)
        .ok()
        .and_then(|contents| contents.lines().last().map(str::to_owned))
        .and_then(|line| serde_json::from_str::<Value>(&line).ok())
        .is_some_and(|record| record["command"] == "DeleteInstance");
    match state.api {
        CloudApi::Azure => {
            if path.contains("/virtualMachines") {
                state.native_reads.fetch_add(1, Ordering::Relaxed);
                if deleted {
                    return response(
                        StatusCode::NOT_FOUND,
                        json!({"error":{"code":"ResourceNotFound"}}),
                    );
                }
                let instances = azure_http_vms(&state.lifecycle_log);
                if path
                    .split('?')
                    .next()
                    .is_some_and(|path| path.ends_with("/virtualMachines"))
                {
                    return response(StatusCode::OK, json!({"value":instances}));
                }
                let requested_name = path
                    .split('?')
                    .next()
                    .and_then(|path| path.rsplit('/').next());
                return response(
                    StatusCode::OK,
                    instances
                        .iter()
                        .find(|instance| instance["name"].as_str() == requested_name)
                        .cloned()
                        .unwrap_or_else(azure_cross_cloud_vm),
                );
            }
            if path.contains("/skus?") {
                state.catalog_reads.fetch_add(1, Ordering::Relaxed);
                return response(StatusCode::OK, azure_system_skus());
            }
            if path.contains("/versions/") {
                state.catalog_reads.fetch_add(1, Ordering::Relaxed);
                let operating_system = if state.invalid_image_metadata.load(Ordering::Relaxed) {
                    state
                        .invalid_image_responses
                        .fetch_add(1, Ordering::Relaxed);
                    "Windows"
                } else {
                    "Linux"
                };
                return response(
                    StatusCode::OK,
                    json!({"properties":{"architecture":"x64","osDiskImage":{"operatingSystem":operating_system},"plan":null}}),
                );
            }
            if path.contains("/api/retail/prices") {
                state.quote_reads.fetch_add(1, Ordering::Relaxed);
                if state.quote_api_failure.load(Ordering::Relaxed) {
                    state.quote_api_failures.fetch_add(1, Ordering::Relaxed);
                    return response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        json!({"error":{"code":"FixtureUnavailable"}}),
                    );
                }
                return response(StatusCode::OK, azure_system_prices());
            }
        }
        CloudApi::Gcp => {
            if path.contains("/machineTypes")
                || path.contains("/global/images/")
                || path.contains("/diskTypes")
            {
                state.catalog_reads.fetch_add(1, Ordering::Relaxed);
                if path.contains("/machineTypes/") {
                    return response(StatusCode::OK, gcp_system_machine());
                }
                if path.contains("/machineTypes") {
                    return response(StatusCode::OK, json!({"items":[gcp_system_machine()]}));
                }
                if path.contains("/global/images/") {
                    return response(StatusCode::OK, gcp_system_image());
                }
                return response(
                    StatusCode::OK,
                    json!({"items":[{"name":"pd-balanced","selfLink":"https://www.googleapis.com/compute/v1/projects/system-project/zones/us-central1-a/diskTypes/pd-balanced","validDiskSize":"10GB-65536GB"}]}),
                );
            }
            if path.contains("/skus?") {
                state.quote_reads.fetch_add(1, Ordering::Relaxed);
                return response(StatusCode::OK, gcp_system_prices());
            }
            if path.contains("/instances") {
                state.native_reads.fetch_add(1, Ordering::Relaxed);
                if deleted {
                    return response(StatusCode::OK, json!({"items":[]}));
                }
                return response(StatusCode::OK, json!({"items":[gcp_cross_cloud_vm()]}));
            }
        }
    }
    response(
        StatusCode::NOT_FOUND,
        json!({"error":"unexpected cloud fixture request","path":path}),
    )
}

pub(super) fn azure_cross_cloud_vm() -> Value {
    json!({"id":"/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/runner-azure-system/providers/Microsoft.Compute/virtualMachines/runner-azure-system","name":"runner-azure-system","location":"eastus","tags":{"garm-controller-id":CONTROLLER_ID,"garm-pool-id":"pool-system-203"},"properties":{"vmId":"azure-vm-system-003","instanceView":{"statuses":[{"code":"PowerState/running"}]}}})
}

pub(super) fn azure_http_vms(lifecycle_log: &std::path::Path) -> Vec<Value> {
    let instance_path = PathBuf::from(format!("{}.instances.json", lifecycle_log.display()));
    let child_instances = fs::read_to_string(instance_path)
        .ok()
        .and_then(|document| serde_json::from_str::<Vec<Value>>(&document).ok())
        .unwrap_or_default();
    let mut instances = vec![azure_cross_cloud_vm()];
    for row in &child_instances {
        let instance = &row["instance"];
        let Some(pool_id) = row["pool_id"].as_str() else {
            continue;
        };
        let Some(name) = instance["name"].as_str() else {
            continue;
        };
        let resource_id = format!(
            "/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/{name}/providers/Microsoft.Compute/virtualMachines/{name}"
        );
        if let Some(existing) = instances
            .iter_mut()
            .find(|existing| existing["id"] == resource_id)
        {
            existing["tags"]["garm-pool-id"] = json!(pool_id);
            continue;
        }
        instances.push(json!({
            "id":resource_id,
            "name":name,
            "location":"eastus",
            "tags":{"garm-controller-id":CONTROLLER_ID,"garm-pool-id":pool_id},
            "properties":{"vmId":azure_fixture_vm_id(name),"instanceView":{"statuses":[{"code":"PowerState/running"}]}}
        }));
    }
    instances
}

pub(super) fn azure_fixture_vm_id(name: &str) -> String {
    if name == "runner-azure-system" {
        "azure-vm-system-003".to_owned()
    } else {
        format!("azure-vm-fixture-{name}")
    }
}

pub(super) fn azure_system_skus() -> Value {
    json!({"value":[{"name":"Standard_D8s_v5","family":"Dsv5","resourceType":"virtualMachines","locations":["eastus"],"capabilities":[{"name":"CpuArchitectureType","value":"x64"},{"name":"vCPUs","value":"8"},{"name":"vCPUsAvailable","value":"8"},{"name":"MemoryGB","value":"32"},{"name":"MaxDataDiskCount","value":"8"},{"name":"UncachedDiskIOPS","value":"20000"},{"name":"UncachedDiskBytesPerSecond","value":"60000000"}],"restrictions":[]}]})
}

pub(super) fn azure_system_prices() -> Value {
    json!({"Items":[
        {"serviceName":"Virtual Machines","productName":"Virtual Machines Dsv5 Series","skuName":"D8s v5","armSkuName":"Standard_D8s_v5","meterName":"D8s v5","armRegionName":"eastus","unitOfMeasure":"1 Hour","type":"Consumption","isPrimaryMeterRegion":true,"effectiveStartDate":"2026-01-01T00:00:00Z","currencyCode":"USD","tierMinimumUnits":0,"retailPrice":"0.20","unitPrice":"0.20","meterId":"vm"},
        {"serviceName":"Storage","productName":"Standard HDD Managed Disks","skuName":"S6","armSkuName":"Standard_D2s_v3","meterName":"S6","armRegionName":"eastus","unitOfMeasure":"1/Month","type":"Consumption","isPrimaryMeterRegion":true,"effectiveStartDate":"2026-01-01T00:00:00Z","currencyCode":"USD","tierMinimumUnits":0,"retailPrice":"3.00","unitPrice":"3.00","meterId":"disk"}
    ],"NextPageLink":null})
}

pub(super) fn gcp_system_machine() -> Value {
    json!({"name":"n2-standard-8","guestCpus":8,"memoryMb":32768,"architecture":"X86_64","isSharedCpu":false,"accelerators":[],"bundledLocalSsds":[]})
}

pub(super) fn gcp_system_image() -> Value {
    json!({"selfLink":"https://www.googleapis.com/compute/v1/projects/system-project/global/images/ubuntu-24","architecture":"X86_64","diskSizeGb":"20","status":"READY","licenses":[]})
}

pub(super) fn gcp_cross_cloud_vm() -> Value {
    json!({"id":"1234567890123456789","name":"runner-gcp-system","status":"RUNNING","zone":"https://www.googleapis.com/compute/v1/projects/system-project/zones/us-central1-a","labels":{"garmcontrollerid":CONTROLLER_ID,"garmpoolid":"pool-system-202"},"metadata":{"items":[{"key":"runner_name","value":"runner-gcp-system"}]}})
}

pub(super) fn gcp_system_prices() -> Value {
    json!({"skus":[
        {"name":"services/6F81-5844-456A/skus/CPU-N2-ONDEMAND","description":"N2 Instance Core","category":{"resourceFamily":"Compute","resourceGroup":"CPU","usageType":"OnDemand"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":20000000}}]}}]},
        {"name":"services/6F81-5844-456A/skus/RAM-N2-ONDEMAND","description":"N2 Instance RAM","category":{"resourceFamily":"Compute","resourceGroup":"RAM","usageType":"OnDemand"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":5000000}}]}}]},
        {"name":"services/6F81-5844-456A/skus/PD-BALANCED","description":"Balanced PD","category":{"resourceFamily":"Storage","resourceGroup":"PD","usageType":"OnDemand"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.mo","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":100000}}]}}]}
    ]})
}
