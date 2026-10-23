use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use http_body_util::BodyExt;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use outrider::{
    config::{AwsCatalogBinding, AwsImageBinding, AwsNetworkBinding, ConfigPatch, ControlSettings},
    providers::{ProviderBinding, PurchasingCapability},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

const AWS_CHILD_CONFIG: &str = "region = \"us-east-1\"\nsubnet_id = \"subnet-123\"\n[credentials]\ncredential_type = \"static\"\n[credentials.static]\naccess_key_id = \"AKIDTESTAWSCONTRACT\"\nsecret_access_key = \"secret-test-only-not-a-real-key\"\n";

pub(super) struct AwsServer {
    pub(super) endpoint: String,
    pub(super) compute_quote_requests: Arc<AtomicU64>,
    pub(super) native_visible: Arc<AtomicBool>,
    pub(super) collision_native_observations: Arc<AtomicU64>,
    pub(super) native_inspection_mode: Arc<AtomicU64>,
    pub(super) native_inspections: Arc<AtomicU64>,
    pub(super) native_inspection_delay_ms: Arc<AtomicU64>,
    pub(super) delayed_native_inspections: Arc<AtomicU64>,
    pub(super) image_requests: Arc<AtomicU64>,
    pub(super) delayed_image_requests: Arc<AtomicU64>,
    pub(super) image_delay_on_request: Arc<AtomicU64>,
    pub(super) image_request_delay_ms: Arc<AtomicU64>,
    pub(super) request_trace: Arc<Mutex<Vec<String>>>,
    pub(super) quote_outage: Arc<AtomicBool>,
    pub(super) spot_event_time: Arc<Mutex<String>>,
    pub(super) spot_conflict: Arc<AtomicBool>,
    pub(super) spot_slow_first: Arc<AtomicBool>,
    pub(super) task: tokio::task::JoinHandle<std::io::Result<()>>,
}

#[derive(Clone)]
pub(super) struct AwsFixtureState {
    request_quote_count: Arc<AtomicU64>,
    native_ownership: NativeOwnership,
    request_native_visible: Arc<AtomicBool>,
    request_collision_observations: Arc<AtomicU64>,
    request_inspection_mode: Arc<AtomicU64>,
    request_native_inspections: Arc<AtomicU64>,
    request_native_inspection_delay_ms: Arc<AtomicU64>,
    request_delayed_native_inspections: Arc<AtomicU64>,
    request_image_requests: Arc<AtomicU64>,
    request_delayed_image_requests: Arc<AtomicU64>,
    request_image_delay_on_request: Arc<AtomicU64>,
    request_image_request_delay_ms: Arc<AtomicU64>,
    request_trace: Arc<Mutex<Vec<String>>>,
    request_quote_outage: Arc<AtomicBool>,
    request_subnet_metadata_changes: Arc<AtomicBool>,
    request_subnet_count: Arc<AtomicU64>,
    pub(super) spot_event_time: Arc<Mutex<String>>,
    pub(super) spot_conflict: Arc<AtomicBool>,
    pub(super) spot_slow_first: Arc<AtomicBool>,
}

#[derive(Clone)]
pub(crate) struct NativeOwnership {
    pub(super) controller_id: String,
    pub(super) pool_id: String,
    pub(super) lifecycle_log: Arc<Mutex<Option<PathBuf>>>,
    pub(super) slow_next_request: Option<Arc<AtomicBool>>,
}

pub(super) enum SubnetMetadataBehavior {
    Stable,
    ChangesAfterInitialLookup,
}

impl From<bool> for SubnetMetadataBehavior {
    fn from(changes_after_initial_lookup: bool) -> Self {
        if changes_after_initial_lookup {
            Self::ChangesAfterInitialLookup
        } else {
            Self::Stable
        }
    }
}

pub(crate) struct JournalFixture {
    pub(crate) directory: PathBuf,
    database: PathBuf,
}

impl JournalFixture {
    pub(crate) fn new() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "outrider-allocation-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&directory)
            .unwrap_or_else(|_| panic!("create allocation journal fixture"));
        let database = directory.join("journal.sqlite3");
        Self {
            directory,
            database,
        }
    }

    pub(crate) fn journal(&self) -> outrider::journal::Journal {
        outrider::journal::Journal::open(&self.database).unwrap_or_else(|error| {
            panic!(
                "open allocation journal at {}: {error:?}",
                self.database.display()
            )
        })
    }
}

impl Drop for JournalFixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.directory) {
            eprintln!("could not remove allocation fixture: {error}");
        }
    }
}

pub(super) fn describe_instances_response(
    request_body: &str,
    native_visible: bool,
    native_ownership: &NativeOwnership,
    collision_observations: &AtomicU64,
    inspection_mode: &AtomicU64,
    native_inspections: &AtomicU64,
    spot: bool,
) -> String {
    let inspection = request_body.contains("InstanceId.1");
    if inspection {
        native_inspections.fetch_add(1, Ordering::Relaxed);
    }

    if (inspection && inspection_mode.load(Ordering::Relaxed) == 0)
        || (!inspection && !native_visible)
    {
        return "<DescribeInstancesResponse/>".to_owned();
    }
    if request_body.contains("runner-collision") {
        collision_observations.fetch_add(1, Ordering::Relaxed);
    }
    let is_poison =
        request_body.contains("runner-poison") || request_body.contains("i-synthetic-003");
    let is_second =
        request_body.contains("runner-second") || request_body.contains("i-synthetic-002");
    let native_id = if is_poison {
        "i-synthetic-003"
    } else if is_second {
        "i-synthetic-002"
    } else {
        "i-synthetic-001"
    };
    let name = if is_poison {
        "runner-poison"
    } else if is_second {
        "runner-second"
    } else if request_body.contains("runner-collision") {
        "runner-collision"
    } else {
        "runner-old-image"
    };
    let controller = if inspection && inspection_mode.load(Ordering::Relaxed) == 3 {
        "foreign-controller"
    } else {
        native_ownership.controller_id.as_str()
    };
    let pool = if inspection && inspection_mode.load(Ordering::Relaxed) == 4 {
        "foreign-pool"
    } else {
        native_ownership.pool_id.as_str()
    };
    let lifecycle_state = native_ownership
        .lifecycle_log
        .lock()
        .ok()
        .and_then(|path| path.clone())
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|log| {
            log.lines()
                .last()
                .and_then(|line| serde_json::from_str::<Value>(line).ok())
        })
        .and_then(|record| record["command"].as_str().map(str::to_owned));
    if lifecycle_state.as_deref() == Some("DeleteInstance") {
        return "<DescribeInstancesResponse/>".to_owned();
    }
    let instance_state = if lifecycle_state.as_deref() == Some("StopInstance") {
        "stopped"
    } else {
        "running"
    };
    let state_code = if instance_state == "stopped" { 80 } else { 16 };
    let spot_identity = if spot {
        if is_second {
            "<instanceLifecycle>spot</instanceLifecycle><spotInstanceRequestId>sir-observed-2</spotInstanceRequestId>"
        } else {
            "<instanceLifecycle>spot</instanceLifecycle><spotInstanceRequestId>sir-observed</spotInstanceRequestId>"
        }
    } else {
        ""
    };
    format!(
        "<DescribeInstancesResponse><reservationSet><item><ownerId>synthetic-account</ownerId><instancesSet><item><instanceId>{native_id}</instanceId><imageId>oldAMI</imageId><instanceType>m7i.xlarge</instanceType><subnetId>subnet-123</subnetId>{spot_identity}<placement><availabilityZone>us-east-1a</availabilityZone></placement><instanceState><code>{state_code}</code><name>{instance_state}</name></instanceState><tagSet><item><key>Name</key><value>{name}</value></item><item><key>GARM_CONTROLLER_ID</key><value>{controller}</value></item><item><key>GARM_POOL_ID</key><value>{pool}</value></item></tagSet></item></instancesSet></item></reservationSet></DescribeInstancesResponse>"
    )
}

async fn delay_fixture_request(request_body: &str, get_products: bool, state: &AwsFixtureState) {
    let slow_next_request = state
        .native_ownership
        .slow_next_request
        .as_ref()
        .is_some_and(|slow_next_request| slow_next_request.swap(false, Ordering::Relaxed));
    let delayed_native_inspection = request_body.contains("DescribeInstances");
    let native_inspection_delay_ms = state
        .request_native_inspection_delay_ms
        .load(Ordering::Relaxed)
        * u64::from(delayed_native_inspection);
    state.request_delayed_native_inspections.fetch_add(
        u64::from(delayed_native_inspection && native_inspection_delay_ms > 0),
        Ordering::Relaxed,
    );
    let delayed_image_request = request_body.contains("DescribeImages");
    let image_request_number = state
        .request_image_requests
        .fetch_add(u64::from(delayed_image_request), Ordering::Relaxed)
        + u64::from(delayed_image_request);
    let trace_entry = if delayed_image_request {
        format!("DescribeImages #{image_request_number}")
    } else if get_products {
        if request_body.contains("Compute Instance") {
            "GetProducts compute".to_owned()
        } else {
            "GetProducts noncompute".to_owned()
        }
    } else {
        request_body
            .split_whitespace()
            .next()
            .unwrap_or("unknown")
            .to_owned()
    };
    state
        .request_trace
        .lock()
        .unwrap_or_else(|_| panic!("AWS request trace lock"))
        .push(trace_entry);
    let image_delay_on_request = state.request_image_delay_on_request.load(Ordering::Relaxed);
    let image_request_delay_ms = state.request_image_request_delay_ms.load(Ordering::Relaxed)
        * u64::from(delayed_image_request && image_delay_on_request == image_request_number);
    state
        .request_delayed_image_requests
        .fetch_add(u64::from(image_request_delay_ms > 0), Ordering::Relaxed);
    let delay_ms = (1500 * u64::from(slow_next_request))
        .max(native_inspection_delay_ms)
        .max(image_request_delay_ms);
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

pub(super) async fn aws_response(
    request: hyper::Request<Incoming>,
    state: AwsFixtureState,
    spot: bool,
) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, std::io::Error> {
    let (parts, body) = request.into_parts();
    let body = body
        .collect()
        .await
        .map_err(std::io::Error::other)?
        .to_bytes();
    let request_body = format!("{} {}", parts.uri, String::from_utf8_lossy(&body));
    let get_products = parts
        .headers
        .get("x-amz-target")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|target| target.contains("GetProducts"));
    delay_fixture_request(&request_body, get_products, &state).await;

    if request_body.contains("InstanceId.1")
        && state.request_inspection_mode.load(Ordering::Relaxed) == 2
    {
        state
            .request_native_inspections
            .fetch_add(1, Ordering::Relaxed);
        return Ok(hyper::Response::builder()
            .status(503)
            .body(http_body_util::Full::new(bytes::Bytes::from(
                "native inspection unavailable",
            )))
            .unwrap_or_else(|error| panic!("allocation fixture: {error}")));
    }

    if parts.headers.get("x-amz-target").is_some()
        && state.request_quote_outage.load(Ordering::Relaxed)
    {
        return Ok(hyper::Response::builder()
            .status(503)
            .body(http_body_util::Full::new(bytes::Bytes::from(
                "quote unavailable",
            )))
            .unwrap_or_else(|error| panic!("allocation fixture: {error}")));
    }

    let response_body = if request_body.contains("GetCallerIdentity") {
        "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>synthetic-account</Account><Arn>arn:aws:iam::synthetic-account:user/test</Arn><UserId>test</UserId></GetCallerIdentityResult></GetCallerIdentityResponse>".to_owned()
    } else if request_body.contains("DescribeInstances") {
        describe_instances_response(
            &request_body,
            state.request_native_visible.load(Ordering::Relaxed),
            &state.native_ownership,
            &state.request_collision_observations,
            &state.request_inspection_mode,
            &state.request_native_inspections,
            spot,
        )
    } else if request_body.contains("DescribeSpotInstanceRequests") {
        if request_body.contains("=sir-observed")
            && !request_body.contains("sir-observed-2")
            && state.spot_slow_first.load(Ordering::Relaxed)
        {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
        if state.spot_conflict.load(Ordering::Relaxed) {
            return Ok(hyper::Response::builder()
                .status(200)
                .header("content-type", "text/xml")
                .body(http_body_util::Full::new(bytes::Bytes::from("<DescribeSpotInstanceRequestsResponse><spotInstanceRequestSet><item><spotInstanceRequestId>sir-observed</spotInstanceRequestId><instanceId>i-synthetic-001</instanceId><state>closed</state><status><code>instance-terminated-no-capacity</code><updateTime>2026-10-21T16:00:00Z</updateTime><message>synthetic preemption</message></status></item><item><spotInstanceRequestId>sir-observed</spotInstanceRequestId><instanceId>i-synthetic-001</instanceId><state>closed</state><status><code>instance-terminated-by-price</code><updateTime>2026-10-21T16:00:00Z</updateTime><message>conflicting reason</message></status></item></spotInstanceRequestSet></DescribeSpotInstanceRequestsResponse>")))
                .unwrap_or_else(|error| panic!("allocation fixture: {error}")));
        }
        let occurred_at = state
            .spot_event_time
            .lock()
            .unwrap_or_else(|_| panic!("Spot fixture time lock"))
            .clone();
        let (spot_id, instance_id) = if request_body.contains("sir-observed-2") {
            ("sir-observed-2", "i-synthetic-002")
        } else {
            ("sir-observed", "i-synthetic-001")
        };
        format!(
            "<DescribeSpotInstanceRequestsResponse><spotInstanceRequestSet><item><spotInstanceRequestId>{spot_id}</spotInstanceRequestId><instanceId>{instance_id}</instanceId><state>closed</state><status><code>instance-terminated-no-capacity</code><updateTime>{occurred_at}</updateTime><message>synthetic preemption</message></status></item></spotInstanceRequestSet></DescribeSpotInstanceRequestsResponse>"
        )
    } else if request_body.contains("DescribeSpotPriceHistory") {
        "<DescribeSpotPriceHistoryResponse><spotPriceHistorySet><item><instanceType>m7i.xlarge</instanceType><productDescription>Linux/UNIX</productDescription><spotPrice>0.05</spotPrice><availabilityZone>us-east-1a</availabilityZone><timestamp>2026-10-21T15:00:00Z</timestamp></item></spotPriceHistorySet></DescribeSpotPriceHistoryResponse>".to_owned()
    } else if request_body.contains("DescribeImages") {
        let image_id = if request_body.contains("oldAMI") {
            "oldAMI"
        } else {
            "newAMI"
        };
        format!(
            "<DescribeImagesResponse><imagesSet><item><imageId>{image_id}</imageId><imageState>available</imageState><architecture>x86_64</architecture><platformDetails>Linux/UNIX</platformDetails><usageOperation>RunInstances</usageOperation><rootDeviceName>/dev/xvda</rootDeviceName><blockDeviceMapping><item><deviceName>/dev/xvda</deviceName><ebs><snapshotId>snap-123</snapshotId><volumeSize>32</volumeSize><deleteOnTermination>true</deleteOnTermination></ebs></blockDeviceMapping></item></imagesSet></DescribeImagesResponse>"
        )
    } else if request_body.contains("DescribeSubnets") {
        let request_count = state.request_subnet_count.fetch_add(1, Ordering::Relaxed);
        let map_public_ip = if request_count > 0
            && state
                .request_subnet_metadata_changes
                .load(Ordering::Relaxed)
        {
            "false"
        } else {
            "true"
        };
        format!(
            "<DescribeSubnetsResponse><subnetSet><item><subnetId>subnet-123</subnetId><availabilityZone>us-east-1a</availabilityZone><mapPublicIpOnLaunch>{map_public_ip}</mapPublicIpOnLaunch></item></subnetSet></DescribeSubnetsResponse>"
        )
    } else if request_body.contains("DescribeInstanceTypes") {
        "<DescribeInstanceTypesResponse><instanceTypeSet><item><instanceType>m7i.xlarge</instanceType><processorInfo><supportedArchitectures><item>x86_64</item></supportedArchitectures></processorInfo><vCpuInfo><defaultVCpus>8</defaultVCpus></vCpuInfo><memoryInfo><sizeInMiB>16384</sizeInMiB></memoryInfo><ebsInfo><ebsOptimizedSupport>default</ebsOptimizedSupport><ebsOptimizedInfo><maximumIops>5000</maximumIops><maximumThroughputInMBps>250.0</maximumThroughputInMBps></ebsOptimizedInfo></ebsInfo><instanceStorageSupported>false</instanceStorageSupported></item></instanceTypeSet></DescribeInstanceTypesResponse>".to_owned()
    } else if request_body.contains("DescribeInstanceTypeOfferings") {
        "<DescribeInstanceTypeOfferingsResponse><instanceTypeOfferingSet><item><instanceType>m7i.xlarge</instanceType><location>us-east-1a</location><locationType>availability-zone</locationType></item></instanceTypeOfferingSet></DescribeInstanceTypeOfferingsResponse>".to_owned()
    } else if parts
        .headers
        .get("x-amz-target")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|target| target.contains("GetProducts"))
    {
        state.request_quote_count.fetch_add(1, Ordering::Relaxed);
        let product = |attributes: Value, unit: &str, rate: &str| {
            json!({
        "product":{"sku":attributes["sku"],"productFamily":attributes["productFamily"],"attributes":attributes},
        "serviceCode":"AmazonEC2","terms":{"OnDemand":{"offer":{"effectiveDate":"2026-01-01T00:00:00Z","priceDimensions":{"dimension":{"unit":unit,"beginRange":"0","endRange":"Inf","appliesTo":[],"pricePerUnit":{"USD":rate}}}}}}
    }).to_string()
        };
        let body = String::from_utf8_lossy(&body);
        if body.contains("Compute Instance") {
            json!({"PriceList":[product(json!({"sku":"compute-sku","productFamily":"Compute Instance","servicecode":"AmazonEC2","instanceType":"m7i.xlarge","location":"US East (N. Virginia)","regionCode":"us-east-1","operatingSystem":"Linux","operation":"RunInstances","usagetype":"USE1-BoxUsage:m7i.xlarge","preInstalledSw":"NA","tenancy":"Shared","capacitystatus":"Used"}),"Hrs","0.10")]}).to_string()
        } else {
            json!({"PriceList":[
            product(json!({"sku":"compute-sku","productFamily":"Compute Instance","servicecode":"AmazonEC2","instanceType":"m7i.xlarge","location":"US East (N. Virginia)","regionCode":"us-east-1","operatingSystem":"Linux","operation":"RunInstances","usagetype":"USE1-BoxUsage:m7i.xlarge","preInstalledSw":"NA","tenancy":"Shared","capacitystatus":"Used"}),"Hrs","0.10"),
            product(json!({"sku":"storage-sku","productFamily":"Storage","location":"US East (N. Virginia)","regionCode":"us-east-1","volumeApiName":"gp3","usagetype":"USE1-EBS:VolumeUsage.gp3"}),"GB-Mo","0.073"),
            product(json!({"sku":"iops-sku","productFamily":"Provisioned IOPS","volumeApiName":"gp3","location":"US East (N. Virginia)","regionCode":"us-east-1","usagetype":"USE1-EBS:VolumeP-IOPS.gp3"}),"IOPS-Mo","0.0073"),
            product(json!({"sku":"throughput-sku","productFamily":"Provisioned Throughput","volumeApiName":"gp3","location":"US East (N. Virginia)","regionCode":"us-east-1","usagetype":"USE1-EBS:VolumeP-Throughput.gp3"}),"MBps-Mo","0.073"),
            product(json!({"sku":"ipv4-sku","location":"US East (N. Virginia)","regionCode":"us-east-1","group":"VPCPublicIPv4Address","usagetype":"USE1-PublicIPv4:InUseAddress"}),"Hrs","0.005")
        ]}).to_string()
        }
    } else {
        "<DescribeImagesResponse/>".to_owned()
    };
    let is_json = parts.headers.get("x-amz-target").is_some();
    let content_type = if is_json {
        "application/x-amz-json-1.1"
    } else {
        "text/xml"
    };
    Ok::<_, std::io::Error>(
        hyper::Response::builder()
            .status(200)
            .header("content-type", content_type)
            .header("connection", "close")
            .body(http_body_util::Full::new(bytes::Bytes::from(response_body)))
            .unwrap_or_else(|_| panic!("AWS response")),
    )
}

pub(super) async fn serve_aws_connection(
    stream: tokio::net::TcpStream,
    state: AwsFixtureState,
    spot: bool,
) {
    let service = service_fn(move |request: hyper::Request<Incoming>| {
        let state = state.clone();
        async move { aws_response(request, state, spot).await }
    });
    if let Err(error) = http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        eprintln!("AWS fixture connection failed: {error}");
    }
}

pub(super) async fn aws_server(
    initial_native_visible: bool,
    spot: bool,
    native_ownership: Option<NativeOwnership>,
    subnet_metadata_behavior: SubnetMetadataBehavior,
) -> AwsServer {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("bind allocation AWS fixture"));
    let endpoint = format!(
        "http://{}",
        listener
            .local_addr()
            .unwrap_or_else(|_| panic!("AWS fixture address"))
    );
    let compute_quote_requests = Arc::new(AtomicU64::new(0));
    let request_quote_count = compute_quote_requests.clone();
    let native_visible = Arc::new(AtomicBool::new(initial_native_visible));
    let request_native_visible = native_visible.clone();
    let collision_native_observations = Arc::new(AtomicU64::new(0));
    let request_collision_observations = collision_native_observations.clone();
    let native_inspection_mode = Arc::new(AtomicU64::new(0));
    let native_inspections = Arc::new(AtomicU64::new(0));
    let native_inspection_delay_ms = Arc::new(AtomicU64::new(0));
    let delayed_native_inspections = Arc::new(AtomicU64::new(0));
    let image_requests = Arc::new(AtomicU64::new(0));
    let delayed_image_requests = Arc::new(AtomicU64::new(0));
    let image_delay_on_request = Arc::new(AtomicU64::new(0));
    let image_request_delay_ms = Arc::new(AtomicU64::new(0));
    let request_trace = Arc::new(Mutex::new(Vec::new()));
    let request_inspection_mode = native_inspection_mode.clone();
    let request_native_inspections = native_inspections.clone();
    let request_native_inspection_delay_ms = native_inspection_delay_ms.clone();
    let request_delayed_native_inspections = delayed_native_inspections.clone();
    let quote_outage = Arc::new(AtomicBool::new(false));
    let request_quote_outage = quote_outage.clone();
    let subnet_metadata_changes = Arc::new(AtomicBool::new(matches!(
        subnet_metadata_behavior,
        SubnetMetadataBehavior::ChangesAfterInitialLookup
    )));
    let request_subnet_metadata_changes = subnet_metadata_changes.clone();
    let request_subnet_count = Arc::new(AtomicU64::new(0));
    let spot_event_time = Arc::new(Mutex::new(String::new()));
    let request_spot_event_time = spot_event_time.clone();
    let spot_conflict = Arc::new(AtomicBool::new(false));
    let request_spot_conflict = spot_conflict.clone();
    let spot_slow_first = Arc::new(AtomicBool::new(false));
    let request_spot_slow_first = spot_slow_first.clone();
    let request_state = AwsFixtureState {
        request_quote_count,
        native_ownership: native_ownership.unwrap_or_else(|| NativeOwnership {
            controller_id: "controller-1".to_owned(),
            pool_id: "pool-1".to_owned(),
            lifecycle_log: Arc::new(Mutex::new(None)),
            slow_next_request: None,
        }),
        request_native_visible,
        request_collision_observations,
        request_inspection_mode,
        request_native_inspections,
        request_native_inspection_delay_ms,
        request_delayed_native_inspections,
        request_image_requests: image_requests.clone(),
        request_delayed_image_requests: delayed_image_requests.clone(),
        request_image_delay_on_request: image_delay_on_request.clone(),
        request_image_request_delay_ms: image_request_delay_ms.clone(),
        request_trace: request_trace.clone(),
        request_quote_outage,
        request_subnet_metadata_changes,
        request_subnet_count,
        spot_event_time: request_spot_event_time,
        spot_conflict: request_spot_conflict,
        spot_slow_first: request_spot_slow_first,
    };
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(serve_aws_connection(stream, request_state.clone(), spot));
        }
    });
    AwsServer {
        endpoint,
        compute_quote_requests,
        native_visible,
        collision_native_observations,
        native_inspection_mode,
        native_inspections,
        native_inspection_delay_ms,
        delayed_native_inspections,
        image_requests,
        delayed_image_requests,
        image_delay_on_request,
        image_request_delay_ms,
        request_trace,
        quote_outage,
        spot_event_time,
        spot_conflict,
        spot_slow_first,
        task,
    }
}

pub(super) fn aws_child_config(directory: &JournalFixture) -> (PathBuf, String) {
    let path = directory.directory.join("aws-provider.toml");
    fs::write(&path, AWS_CHILD_CONFIG)
        .unwrap_or_else(|_| panic!("write AWS provider configuration"));
    let digest = hex::encode(Sha256::digest(AWS_CHILD_CONFIG.as_bytes()));
    (path, digest)
}

pub(crate) fn allocation_binding(directory: &JournalFixture, mode: &str) -> ProviderBinding {
    let executable_path = directory.directory.join("provider-process");
    outrider_xtask::provider_fixture::install(
        &executable_path,
        &outrider_xtask::provider_fixture::Configuration {
            scenario: if mode == "slow_first_delete" {
                outrider_xtask::provider_fixture::Scenario::Recovery
            } else {
                outrider_xtask::provider_fixture::Scenario::Protocol
            },
            native_locator: None,
            lifecycle_log: None,
        },
    )
    .unwrap_or_else(|_| panic!("install native provider fixture"));
    let executable_sha256 = hex::encode(Sha256::digest(
        fs::read(&executable_path).unwrap_or_else(|_| panic!("read provider fixture")),
    ));
    let (configuration_path, configuration_sha256) = aws_child_config(directory);
    ProviderBinding {
        id: "aws-child".to_owned(),
        cloud: "aws".to_owned(),
        account: "synthetic-account".to_owned(),
        location: "us-east-1".to_owned(),
        executable_path,
        executable_sha256,
        configuration_path,
        configuration_sha256,
        source_revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        expected_version: "fixture-build-1".to_owned(),
        inherited_environment: vec![
            "OUTRIDER_FIXTURE_MODE".to_owned(),
            "OUTRIDER_RECORD".to_owned(),
            "OUTRIDER_LIFECYCLE_LOG".to_owned(),
            "OUTRIDER_CREATE_BARRIER".to_owned(),
            "OUTRIDER_NATIVE_LOCATOR".to_owned(),
        ],
        supported_os: BTreeSet::from(["linux".to_owned()]),
        supported_architectures: BTreeSet::from(["amd64".to_owned()]),
        purchasing_capabilities: BTreeSet::from([PurchasingCapability::OnDemand]),
    }
}

pub(crate) fn allocation_central(binding: &ProviderBinding, image_id: &str) -> ConfigPatch {
    let mut config = ConfigPatch::from_yaml(include_str!("../../../examples/central.yaml"))
        .unwrap_or_else(|_| panic!("central allocation config"));
    config.history = Some(outrider::config::HistorySettings {
        window_seconds: 3600,
        sampling_interval_seconds: 1,
        maximum_gap_seconds: 10,
    });
    let mut alias_binding = binding.clone();
    alias_binding.id = "aws-child-alias".to_owned();
    let fixture_directory = binding
        .executable_path
        .parent()
        .unwrap_or_else(|| panic!("AWS child path has no parent"));
    alias_binding.executable_path = fixture_directory.join("aws-child-alias");
    fs::copy(&binding.executable_path, &alias_binding.executable_path)
        .unwrap_or_else(|_| panic!("copy alias AWS child executable"));
    fs::copy(
        binding.executable_path.with_extension("json"),
        alias_binding.executable_path.with_extension("json"),
    )
    .unwrap_or_else(|_| panic!("copy alias AWS child scenario"));
    alias_binding.configuration_path = fixture_directory.join("aws-child-alias.toml");
    fs::copy(
        &binding.configuration_path,
        &alias_binding.configuration_path,
    )
    .unwrap_or_else(|_| panic!("copy alias AWS child configuration"));
    config.provider_bindings = Some(vec![binding.clone(), alias_binding]);
    let catalog = AwsCatalogBinding {
        id: "aws-east".to_owned(),
        provider_binding_id: binding.id.clone(),
        account_id: binding.account.clone(),
        region: binding.location.clone(),
        pricing_location: "US East (N. Virginia)".to_owned(),
        credential_reference: format!("provider-binding:{}:configuration", binding.id),
        locations: BTreeMap::from([(
            "us-east-1".to_owned(),
            AwsNetworkBinding {
                availability_zone: "us-east-1a".to_owned(),
                subnet_id: "subnet-123".to_owned(),
                security_group_ids: vec!["sg-123".to_owned()],
            },
        )]),
        images: BTreeMap::from([(
            "ubuntu-24".to_owned(),
            AwsImageBinding {
                image_id: image_id.to_owned(),
                os: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                capabilities: BTreeSet::from(["docker".to_owned()]),
                windows_license_included: false,
                cloud_init: true,
                ec2_launch_agent: false,
            },
        )]),
        storage_classes: BTreeMap::from([("gp3".to_owned(), "gp3".to_owned())]),
    };
    let duplicate_catalog = AwsCatalogBinding {
        id: "aws-east-duplicate".to_owned(),
        provider_binding_id: "aws-child-alias".to_owned(),
        credential_reference: "provider-binding:aws-child-alias:configuration".to_owned(),
        ..catalog.clone()
    };
    config.aws_catalog_bindings = Some(vec![catalog, duplicate_catalog]);
    config.ingress = Some(outrider::config::IngressSettings {
        github_api_origin: "https://api.github.invalid".to_owned(),
        github_token_file: "/missing/github-token".to_owned(),
        garm_api_origin: "https://garm.invalid".to_owned(),
        garm_token_file: "/missing/garm-token".to_owned(),
        controller_id: "controller-1".to_owned(),
        provider_name: "outrider".to_owned(),
        pool_flavor: "m7i.xlarge".to_owned(),
        pool_image: "ubuntu-24".to_owned(),
        runner_prefix: "runner".to_owned(),
        max_dynamic_pools: 10,
        max_runners_per_pool: 10,
        allocation_timeout_seconds: 600,
        bootstrap_timeout_minutes: 20,
        timeout_margin_seconds: 30,
        pool_ttl_seconds: 3600,
        webhook_max_bytes: 1024 * 1024,
        repositories: BTreeMap::new(),
    });
    config.control = Some(ControlSettings {
        listen_address: "127.0.0.1:0".to_owned(),
        database_path: "/unused".to_owned(),
        auth_token_file: "/unused".to_owned(),
        provider_timeout_seconds: 1,
    });
    config
}
