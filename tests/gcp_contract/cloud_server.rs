use super::identity::{gpu_skus, image_json, machine_json, machine_page, windows_skus};
use std::{
    io,
    sync::{Arc, Mutex},
};

use google_cloud_auth::credentials::{
    AccessToken, AccessTokenCredentialsProvider, CacheableResource, CredentialsProvider, EntityTag,
};
use http_body_util::BodyExt;
use hyper::http::{Extensions, HeaderMap, HeaderValue};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinHandle};

pub(super) const CHILD_CONFIG: &str = "project_id = \"synthetic-project\"\nzone = \"us-central1-a\"\ncredentials_file = \"/tmp/gcp-contract-credentials.json\"\nnetwork_id = \"projects/synthetic-project/global/networks/runners\"\nsubnetwork_id = \"projects/synthetic-project/regions/us-central1/subnetworks/runners\"\nexternal_ip_access = false\nenable_regional_placement = false\n";

#[derive(Debug)]
pub(super) struct StaticCredential;

impl CredentialsProvider for StaticCredential {
    async fn headers(
        &self,
        _extensions: Extensions,
    ) -> std::result::Result<
        CacheableResource<HeaderMap>,
        google_cloud_auth::errors::CredentialsError,
    > {
        let mut headers = HeaderMap::new();
        headers.insert(
            hyper::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer contract-token"),
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

impl AccessTokenCredentialsProvider for StaticCredential {
    async fn access_token(
        &self,
    ) -> std::result::Result<AccessToken, google_cloud_auth::errors::CredentialsError> {
        Ok(AccessToken {
            token: "contract-token".to_owned(),
        })
    }
}

#[derive(Clone)]
pub(super) struct RecordedRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) body: String,
    pub(super) authorization: Option<String>,
}

pub(super) struct MockServer {
    pub(super) endpoint: String,
    pub(super) requests: Arc<Mutex<Vec<RecordedRequest>>>,
    pub(super) task: JoinHandle<io::Result<()>>,
}

#[derive(Clone, Copy)]
pub(super) enum MockShape {
    Standard,
    NativeSpot,
    WindowsSql,
    GpuSpot,
    BundledSsd,
}

#[derive(Clone)]
pub(super) struct MockRoutes {
    pub(super) shape: MockShape,
    pub(super) advice_status: u16,
    pub(super) advice_body: String,
    pub(super) instances_status: u16,
    pub(super) instances_body: String,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) async fn start_server() -> io::Result<MockServer> {
    start_server_with_routes(MockRoutes {
        shape: MockShape::Standard,
        advice_status: 200,
        advice_body: r#"{"recommendations":[{"scores":{"obtainability":0},"shards":[{"zone":"us-central1-a","machineType":"n2-standard-4","instanceCount":1,"provisioningModel":"SPOT"}]}]}"#.to_owned(),
        instances_status: 200,
        instances_body: r#"{"items":[{"id":"1234567890123456789","name":"runner-native","status":"RUNNING","zone":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a","labels":{"garmcontrollerid":"controller-1","garmpoolid":"pool-1"},"metadata":{"items":[{"key":"runner_name","value":"runner-journal"}]}}]}"#.to_owned(),
    }).await
}

pub(super) async fn start_server_with_routes(routes: MockRoutes) -> io::Result<MockServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let routes = Arc::new(routes);
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let recorded = recorded.clone();
            let routes = routes.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: hyper::Request<Incoming>| {
                    let recorded = recorded.clone();
                    let routes = routes.clone();
                    handle_mock_request(request, recorded, routes)
                });
                let _result = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    Ok(MockServer {
        endpoint,
        requests,
        task,
    })
}

pub(super) async fn handle_mock_request(
    request: hyper::Request<Incoming>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
    routes: Arc<MockRoutes>,
) -> io::Result<hyper::Response<http_body_util::Full<bytes::Bytes>>> {
    let (parts, body) = request.into_parts();
    let body = body.collect().await.map_err(io::Error::other)?.to_bytes();
    let method = parts.method;
    let path = parts.uri.to_string();
    recorded
        .lock()
        .map_err(|_| io::Error::other("GCP request capture lock poisoned"))?
        .push(RecordedRequest {
            method: method.to_string(),
            path: path.clone(),
            body: String::from_utf8_lossy(&body).into_owned(),
            authorization: parts
                .headers
                .get(hyper::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        });
    let (status, response_body) = response_for(method.as_ref(), &path, &routes);
    hyper::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-goog-request-id", "request-contract-1")
        .header("connection", "close")
        .body(http_body_util::Full::new(bytes::Bytes::from(response_body)))
        .map_err(io::Error::other)
}

pub(super) fn response_for(method: &str, path: &str, routes: &MockRoutes) -> (u16, String) {
    if path.contains("/operations") {
        if routes.advice_body == "fail-second-operation-page" {
            return if path.contains("pageToken=") {
                (503, r#"{"error":{"status":"UNAVAILABLE"}}"#.to_owned())
            } else {
                (200, r#"{"items":[],"nextPageToken":"second"}"#.to_owned())
            };
        }
        return (routes.advice_status, routes.advice_body.clone());
    }
    if path.contains("/skus?") {
        if matches!(routes.shape, MockShape::NativeSpot) {
            return (200, include_str!("predefined-spot-prices.json").to_owned());
        }

        if matches!(routes.shape, MockShape::WindowsSql) {
            return (200, windows_skus(path.contains("pageToken=")));
        }
        if matches!(routes.shape, MockShape::GpuSpot) {
            return (200, gpu_skus(path.contains("pageToken=")));
        }
        let first = r#"{"skus":[{"name":"services/6F81-5844-456A/skus/CPU-N2-SPOT","description":"N2 Instance Core","category":{"resourceFamily":"Compute","resourceGroup":"CPU","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":10000000}}]}},{"effectiveTime":"2030-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"9","nanos":0}}]}}]}],"nextPageToken":"pricing/page"}"#;
        let second = r#"{"skus":[{"name":"services/6F81-5844-456A/skus/RAM-N2-SPOT","description":"N2 Instance RAM","category":{"resourceFamily":"Compute","resourceGroup":"RAM","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":5000000}}]}}]},{"name":"services/6F81-5844-456A/skus/PD-BALANCED","description":"Balanced PD","category":{"resourceFamily":"Storage","resourceGroup":"PD","usageType":"OnDemand"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.mo","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":100000}}]}}]},{"name":"services/6F81-5844-456A/skus/LOCAL-SSD-SYNTHETIC","description":"Local SSD attached capacity","category":{"resourceFamily":"Storage","resourceGroup":"Local SSD","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"GiBy.h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":1000000}}]}}]},{"name":"services/6F81-5844-456A/skus/CPU-N2-COMMIT-1Y","description":"N2 Instance Core","category":{"resourceFamily":"Compute","resourceGroup":"CPU","usageType":"Commit1Yr"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":1}}]}}]},{"name":"services/6F81-5844-456A/skus/CPU-N2D-SPOT","description":"N2D Instance Core","category":{"resourceFamily":"Compute","resourceGroup":"CPU","usageType":"Preemptible"},"serviceRegions":["us-central1"],"pricingInfo":[{"effectiveTime":"2025-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"0","nanos":1}}]}}]}]}"#;
        return (
            200,
            if path.contains("pageToken=") {
                second.to_owned()
            } else {
                first.to_owned()
            },
        );
    }
    if path.contains("/machineTypes") {
        if routes.advice_body == "repeat-compute-page-token" {
            return (
                200,
                r#"{"items":[],"nextPageToken":"repeat/page"}"#.to_owned(),
            );
        }
        let machine_name = match routes.shape {
            MockShape::Standard => "n2-standard-4",
            MockShape::NativeSpot => "e2-standard-8",
            MockShape::WindowsSql => "n2-standard-2",
            MockShape::GpuSpot => "a2-highgpu-1g",
            MockShape::BundledSsd => "n2-local-ssd-synthetic",
        };
        if path.ends_with(&format!("/machineTypes/{machine_name}")) {
            return (200, machine_json(routes.shape));
        }
        if path.contains("pageToken=") {
            return (200, machine_page(routes.shape, false));
        }
        return (200, machine_page(routes.shape, true));
    }
    if path.contains("/global/images/") {
        return (200, image_json(routes.shape));
    }
    if path.contains("/diskTypes") {
        return (
            200,
            r#"{"items":[{"name":"pd-balanced","selfLink":"https://www.googleapis.com/compute/v1/projects/synthetic-project/zones/us-central1-a/diskTypes/pd-balanced","validDiskSize":"10GB-65536GB"}]}"#.to_owned(),
        );
    }
    if path.contains("/advice/capacity") && method == "POST" {
        return (routes.advice_status, routes.advice_body.clone());
    }
    if path.contains("/instances") {
        return (routes.instances_status, routes.instances_body.clone());
    }
    (404, r#"{"error":"unexpected request"}"#.to_owned())
}
