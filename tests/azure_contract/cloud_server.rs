use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use azure_core::{
    credentials::{AccessToken, TokenCredential, TokenRequestOptions},
    time::OffsetDateTime,
};
use outrider::{
    cloud::{
        CatalogQuery,
        azure::{AzureBackend, AzureImage, AzureLocation, AzureSettings},
    },
    config::{AzureImageLicensing, ConfigPatch, RunnerProfile},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

pub(super) struct MockResponse {
    pub(super) status: u16,
    pub(super) body: String,
}

pub(super) struct MockRoute {
    pub(super) path_contains: Option<String>,
    pub(super) response: MockResponse,
    pub(super) headers: Vec<(String, String)>,
}

pub(super) struct CapturedRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) authorization: Option<String>,
}

pub(super) struct MockServer {
    pub(super) endpoint: String,
    pub(super) requests: Arc<Mutex<Vec<CapturedRequest>>>,
    pub(super) task: tokio::task::JoinHandle<io::Result<()>>,
}

#[derive(Debug)]
pub(super) struct TestCredential;

#[async_trait]
impl TokenCredential for TestCredential {
    async fn get_token(
        &self,
        _scopes: &[&str],
        _options: Option<TokenRequestOptions<'_>>,
    ) -> azure_core::Result<AccessToken> {
        Ok(AccessToken::new(
            "local-test-token",
            OffsetDateTime::from_unix_timestamp(4_102_444_800)
                .unwrap_or_else(|_| panic!("valid timestamp")),
        ))
    }
}

pub(super) async fn start_server(responses: Vec<MockResponse>) -> io::Result<MockServer> {
    start_server_with_headers(
        responses
            .into_iter()
            .map(|response| (response, Vec::new()))
            .collect(),
    )
    .await
}

pub(super) async fn start_server_with_headers(
    responses: Vec<(MockResponse, Vec<(String, String)>)>,
) -> io::Result<MockServer> {
    start_server_with_routes(
        responses
            .into_iter()
            .map(|(response, headers)| MockRoute {
                path_contains: None,
                response,
                headers,
            })
            .collect(),
    )
    .await
}

pub(super) async fn start_server_with_routes(mut routes: Vec<MockRoute>) -> io::Result<MockServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let response_endpoint = endpoint.clone();
    let task = tokio::spawn(async move {
        for _ in 0..routes.len() {
            let (mut stream, _) = listener.accept().await?;
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let count = stream.read(&mut chunk).await?;
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&bytes);
            let mut lines = request.lines();
            let first = lines.next().unwrap_or_default();
            let mut fields = first.split_whitespace();
            let method = fields.next().unwrap_or_default().to_owned();
            let path = fields.next().unwrap_or_default().to_owned();
            let authorization = lines.find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            });
            let route_index = routes
                .iter()
                .position(|route| {
                    route
                        .path_contains
                        .as_ref()
                        .is_none_or(|part| path.contains(part))
                })
                .ok_or_else(|| io::Error::other("unexpected Azure mock request path"))?;
            let MockRoute {
                response, headers, ..
            } = routes.remove(route_index);
            recorded
                .lock()
                .map_err(|_| io::Error::other("request capture lock poisoned"))?
                .push(CapturedRequest {
                    method,
                    path: path.clone(),
                    authorization,
                });
            let reason = if response.status == 200 {
                "OK"
            } else {
                "Bad Request"
            };
            let mut rendered_body = response.body.replace("{{endpoint}}", &response_endpoint);
            if path.starts_with("/api/retail/prices?")
                && let Ok(decoded) = urlencoding::decode(&path)
                && decoded.contains("$filter=")
                && !decoded
                    .contains("serviceName eq 'Virtual Network' and productName eq 'IP Addresses'")
                && let Ok(mut page) = serde_json::from_str::<Value>(&rendered_body)
                && let Some(items) = page["Items"].as_array_mut()
            {
                items.retain(|item| item["productName"] != "IP Addresses");
                rendered_body = page.to_string();
            }

            let body = rendered_body.as_bytes();
            let extra_headers = headers
                .into_iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect::<String>();
            let head = format!(
                "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra_headers}connection: close\r\n\r\n",
                response.status,
                reason,
                body.len()
            );
            stream.write_all(head.as_bytes()).await?;
            stream.write_all(body).await?;
        }
        Ok(())
    });
    Ok(MockServer {
        endpoint,
        requests,
        task,
    })
}

pub(super) fn child_config() -> Vec<u8> {
    b"location = \"eastus\"\n[credentials]\nsubscription_id = \"11111111-1111-4111-8111-111111111111\"\n[credentials.service_principal]\ntenant_id = \"22222222-2222-4222-8222-222222222222\"\nclient_id = \"33333333-3333-4333-8333-333333333333\"\nclient_secret = \"synthetic-secret\"\n".to_vec()
}

pub(super) fn settings(endpoint: &str, config_path: PathBuf) -> AzureSettings {
    let bytes = std::fs::read(&config_path).unwrap_or_else(|_| panic!("child config exists"));
    AzureSettings {
        catalog_id: "azure-east".to_owned(),
        account_id: "build-account".to_owned(),
        subscription_id: "11111111-1111-4111-8111-111111111111".to_owned(),
        pricing_location: "eastus".to_owned(),
        locations: BTreeMap::from([(
            "eastus".to_owned(),
            AzureLocation {
                location: "eastus".to_owned(),
                vnet_subnet_id: None,
                public_ip_sku: None,
            },
        )]),
        images: BTreeMap::from([(
            "ubuntu-24".to_owned(),
            AzureImage {
                image_id: "Canonical:UbuntuServer:22_04-lts-gen2:22.04.202601010".to_owned(),
                minimum_os_disk_gib: 30,
                minimum_os_disk_source: "operator verified Canonical image documentation"
                    .to_owned(),
                capabilities: BTreeSet::from(["docker".to_owned()]),
                licensing: AzureImageLicensing::NoAdditionalSoftwareCharge,
                licensing_source: "operator verified Canonical image pricing documentation"
                    .to_owned(),
            },
        )]),
        storage_classes: BTreeMap::from([("standard".to_owned(), "Standard_LRS".to_owned())]),
        accelerator_facts: BTreeMap::new(),
        binding_id: "azure-child".to_owned(),
        source_revision: "10659879c5d91979c7f077439bd78e17622452a6".to_owned(),
        binary_sha256: "a".repeat(64),
        configuration_sha256: hex::encode(Sha256::digest(bytes)),
        provider_configuration_path: config_path,
        management_endpoint: endpoint.to_owned(),
        retail_endpoint: endpoint.to_owned(),
    }
}

pub(super) async fn backend(server: &MockServer) -> AzureBackend {
    backend_with_storage_and_licensing(
        server,
        "Standard_LRS",
        AzureImageLicensing::NoAdditionalSoftwareCharge,
        "operator verified Canonical image pricing documentation",
    )
    .await
}

pub(super) async fn premium_backend(server: &MockServer) -> AzureBackend {
    backend_with_storage_and_licensing(
        server,
        "Premium_LRS",
        AzureImageLicensing::NoAdditionalSoftwareCharge,
        "operator verified Canonical image pricing documentation",
    )
    .await
}

pub(super) async fn backend_with_storage_and_licensing(
    server: &MockServer,
    storage_sku: &str,
    licensing: AzureImageLicensing,
    licensing_source: &str,
) -> AzureBackend {
    static NEXT_CONFIG_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "outrider-azure-contract-{}-{}.toml",
        std::process::id(),
        NEXT_CONFIG_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&path, child_config()).unwrap_or_else(|_| panic!("write child config"));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| panic!("HTTP client"));
    let mut settings = settings(&server.endpoint, path);
    settings
        .storage_classes
        .insert("standard".to_owned(), storage_sku.to_owned());
    let image = settings
        .images
        .get_mut("ubuntu-24")
        .unwrap_or_else(|| panic!("configured image"));
    image.licensing = licensing;
    image.licensing_source = licensing_source.to_owned();
    AzureBackend::with_clients(
        settings,
        client,
        Arc::new(TestCredential),
        tokio::time::Instant::now() + Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|_| panic!("valid Azure binding"))
}

pub(super) fn profile() -> RunnerProfile {
    let mut central = ConfigPatch::from_toml(include_str!("../../examples/central.toml"))
        .unwrap_or_else(|_| panic!("central config"));
    let limits = central.limits.as_mut().unwrap_or_else(|| panic!("limits"));
    limits.allowed_clouds.insert("azure".to_owned());
    limits.allowed_accounts.insert("build-account".to_owned());
    limits.allowed_locations.insert("eastus".to_owned());
    limits.allowed_machine_types.insert(
        "azure".to_owned(),
        BTreeSet::from(["Standard_D2s_v3".to_owned()]),
    );
    let mut request = central
        .normalize_profile(None, "linux-large", None)
        .unwrap_or_else(|_| panic!("profile"))
        .profile;
    request.accounts = vec!["build-account".to_owned()];
    request.locations = vec!["eastus".to_owned()];
    request.machine_types =
        BTreeMap::from([("azure".to_owned(), vec!["Standard_D2s_v3".to_owned()])]);
    request.boot_disk.class = "standard".to_owned();
    request.boot_disk.size_gib = 40;
    request
}

pub(super) fn image_response() -> MockResponse {
    image_response_for("Linux", None)
}

pub(super) fn image_response_for(
    operating_system: &str,
    plan: Option<serde_json::Value>,
) -> MockResponse {
    MockResponse {
        status: 200,
        body: serde_json::json!({"properties":{
            "architecture":"x64",
            "osDiskImage":{"operatingSystem":operating_system},
            "plan":plan
        }})
        .to_string(),
    }
}

pub(super) fn sku(name: &str) -> serde_json::Value {
    serde_json::json!({"name":name,"family":"Dsv3","resourceType":"virtualMachines","locations":["eastus"],"capabilities":[
        {"name":"CpuArchitectureType","value":"x64"},{"name":"PremiumIO","value":"True"},{"name":"vCPUs","value":"16"},{"name":"vCPUsAvailable","value":"8"},
        {"name":"MemoryGB","value":"16"},{"name":"MaxDataDiskCount","value":"4"},
        {"name":"UncachedDiskIOPS","value":"20000"},{"name":"UncachedDiskBytesPerSecond","value":"60000000.5"}
    ],"restrictions":[]})
}

pub(super) fn meter(
    service: &str,
    product: &str,
    sku: &str,
    name: &str,
    unit: &str,
    price: &str,
    id: &str,
) -> serde_json::Value {
    let (sku, name) = if service == "Storage" {
        (format!("{sku} LRS"), format!("{name} LRS Disk"))
    } else {
        (sku.to_owned(), name.to_owned())
    };

    serde_json::json!({"serviceName":service,"productName":product,"skuName":sku,"armSkuName":"Standard_D2s_v3","meterName":name,
        "armRegionName":"eastus","unitOfMeasure":unit,"type":"Consumption","isPrimaryMeterRegion":true,
        "effectiveStartDate":"2026-01-01T00:00:00Z","currencyCode":"USD","tierMinimumUnits":0,
        "retailPrice":price,"unitPrice":price,"meterId":id})
}

pub(super) fn retail_response() -> MockResponse {
    let mut low_priority = meter(
        "Virtual Machines",
        "Virtual Machines Dsv3 Series",
        "D2s v3 Low Priority",
        "D2s v3 Low Priority",
        "1 Hour",
        "0.02",
        "compute-low-priority",
    );
    low_priority["skuName"] = serde_json::json!("D2s v3 Low Priority");
    let mut devtest = low_priority.clone();
    devtest["type"] = serde_json::json!("DevTestConsumption");
    MockResponse { status: 200, body: serde_json::json!({"Items":[
        meter("Virtual Machines","Virtual Machines Dsv3 Series","D2s v3","D2s v3","1 Hour","0.10","compute-on-demand"),
        meter("Virtual Machines","Virtual Machines Dsv3 Series Windows","D2s v3 Windows","D2s v3 Windows","1 Hour","0.15","compute-windows"),
        meter("Virtual Machines","Virtual Machines Dsv3 Series","D2s v3 Spot","D2s v3 Spot","1 Hour","0.03","compute-spot"),
        low_priority,
        devtest,
        meter("Storage","Standard HDD Managed Disks","S6","S6","1/Month","3.00","disk-s6"),
        meter("Storage","Premium SSD Managed Disks","P6","P6","1/Month","3.00","disk-p6"),
        meter("Storage","Standard HDD Managed Disks","S10","S10","1/Month","5.89","disk-s10"),
        meter("Storage","Standard HDD Managed Disks","S50","S50","1/Month","100.00","disk-s50"),
        meter("Virtual Network","IP Addresses","Standard","Standard IPv4 Static Public IP","1 Hour","0.005","ip-standard")
    ],"NextPageLink":null}).to_string() }
}

pub(super) fn large_retail_page() -> MockResponse {
    let mut body: Value =
        serde_json::from_str(&retail_response().body).unwrap_or_else(|_| panic!("retail fixture"));
    let items = body["Items"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("retail items"));
    for index in 0..1000 {
        items.push(meter(
            "Unrelated Service",
            "Irrelevant product",
            "Irrelevant SKU",
            &format!("irrelevant meter {index}"),
            "1 Hour",
            "99.00",
            &format!("irrelevant-{index}"),
        ));
    }
    body["NextPageLink"] =
        serde_json::json!("{{endpoint}}/api/retail/prices?api-version=2023-01-01-preview&$skip=2");
    MockResponse {
        status: 200,
        body: body.to_string(),
    }
}

pub(super) fn changed_retail_page(price: &str) -> MockResponse {
    let mut body: Value = serde_json::from_str(&large_retail_page().body)
        .unwrap_or_else(|_| panic!("large Retail fixture"));
    let items = body["Items"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("retail items"));
    let compute = items
        .iter_mut()
        .find(|item| item["meterId"] == "compute-on-demand")
        .unwrap_or_else(|| panic!("compute meter"));
    compute["unitPrice"] = serde_json::json!(price);
    compute["retailPrice"] = serde_json::json!(price);
    MockResponse {
        status: 200,
        body: body.to_string(),
    }
}

pub(super) fn irrelevant_retail_page() -> MockResponse {
    let mut body: Value =
        serde_json::from_str(&retail_response().body).unwrap_or_else(|_| panic!("retail fixture"));
    body["Items"] = serde_json::json!([meter(
        "Unrelated Service",
        "Irrelevant product",
        "Irrelevant SKU",
        "irrelevant continuation",
        "1 Hour",
        "99.00",
        "irrelevant-continuation",
    )]);
    body["NextPageLink"] = Value::Null;
    MockResponse {
        status: 200,
        body: body.to_string(),
    }
}

pub(super) fn vm_response() -> MockResponse {
    MockResponse { status: 200, body: serde_json::json!({
        "id":"/subscriptions/11111111-1111-4111-8111-111111111111/resourceGroups/garm/providers/Microsoft.Compute/virtualMachines/runner-1",
        "name":"runner-1","location":"eastus","tags":{"garm-controller-id":"controller-1","garm-pool-id":"pool-1"},
        "properties":{"provisioningState":"Succeeded","vmId":"00000000-0000-4000-8000-000000000001","instanceView":{"statuses":[{"code":"PowerState/running"}]}}
    }).to_string() }
}

pub(super) fn query(profile: RunnerProfile) -> CatalogQuery {
    CatalogQuery {
        clouds: vec!["azure".to_owned()],
        accounts: vec!["build-account".to_owned()],
        locations: vec!["eastus".to_owned()],
        profile,
    }
}
