use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_ec2::primitives::{DateTime, DateTimeFormat};
use aws_sdk_ec2::{Client as Ec2Client, config::Region, types::InstanceType};
use aws_sdk_pricing::Client as PricingClient;
use aws_sdk_sts::Client as StsClient;
use aws_types::request_id::RequestId;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use tokio::{time::Instant, time::timeout_at};

use super::{
    CatalogQuery, CloudCatalog, CloudError, CloudFailure, InterruptionEvidence, OwnershipScope,
    PlacementIdentity, ProviderInstance,
};

use crate::config::{AwsCatalogBinding, AwsImageBinding};
use crate::policy::{
    AwsLaunchFacts, Candidate, CatalogOffer, PriceComponent, PriceQuote, ProviderLaunchFacts,
    ProviderSelection,
};

use crate::providers::ProviderBinding;

mod billing;
mod catalog;
mod connection;
mod discovery;
mod identity;
mod inventory;
#[cfg(test)]
#[path = "aws/tests.rs"]
mod tests;

#[derive(Clone, Debug)]
pub struct AwsLocation {
    pub region: String,
    pub availability_zone: String,
    pub subnet_id: String,
    pub security_group_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct AwsImage {
    pub image_id: String,
    pub os: String,
    pub architecture: String,
    pub capabilities: BTreeSet<String>,
    pub windows_license_included: bool,
    pub cloud_init: bool,
    pub ec2_launch_agent: bool,
}

#[derive(Clone, Debug)]
pub struct AwsSettings {
    pub catalog_id: String,
    pub account_id: String,
    pub region: String,
    pub pricing_location: String,
    pub locations: BTreeMap<String, AwsLocation>,
    pub images: BTreeMap<String, AwsImage>,
    pub storage_classes: BTreeMap<String, String>,
    pub include_spot_placement_scores: bool,
    pub ec2_endpoint_url: Option<String>,
    pub pricing_endpoint_url: Option<String>,
    pub sts_endpoint_url: Option<String>,
    pub binding_id: String,
    pub source_revision: String,
    pub binary_sha256: String,
    pub configuration_sha256: String,
    pub provider_configuration_path: PathBuf,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AwsChildConfiguration {
    credentials: AwsChildCredentials,
    subnet_id: String,
    region: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AwsChildCredentials {
    credential_type: String,
    #[serde(rename = "static")]
    static_credentials: Option<AwsStaticCredentials>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AwsStaticCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

pub struct AwsBackend {
    settings: AwsSettings,
    ec2: Ec2Client,
    pricing: PricingClient,
}

struct AwsCatalogInventory {
    machine_types: Vec<aws_sdk_ec2::types::InstanceTypeInfo>,
    offered: BTreeSet<(String, String)>,
    locations: Vec<AwsCatalogLocation>,
    spot_hints: BTreeMap<String, crate::policy::NativeCapacityHint>,
}

struct AwsCatalogLocation {
    alias: String,
    location: AwsLocation,
    subnet: AwsSubnetFacts,
}

struct AwsSubnetFacts {
    availability_zone: String,
    map_public_ip_on_launch: bool,
}

struct AwsGpuShape {
    vendor: Option<String>,
    model: Option<String>,
    count: u32,
    memory_gib: Option<Decimal>,
}

struct AwsMachineShape {
    machine_type: String,
    family: Option<String>,
    vcpu: u32,
    memory_gib: Decimal,
    gpu: AwsGpuShape,
    local_scratch_gib: Decimal,
    ebs_iops: u32,
    ebs_throughput_mib_s: Decimal,
}

struct AwsComputeQuote {
    rate: Option<Decimal>,
    effective_at: Option<u64>,
    request_ids: Vec<String>,
}

struct AwsDiskQuote {
    hourly_rate: Option<Decimal>,
    effective_at: Option<u64>,
    request_ids: Vec<String>,
}

struct AwsPriceProducts {
    products: Vec<String>,
    request_ids: Vec<String>,
}

struct AwsQuotedPrices {
    compute: AwsComputeQuote,
    disk: AwsDiskQuote,
    public_rate: Option<Decimal>,
    public_effective_at: Option<u64>,
    provider_request_ids: Vec<String>,
}

fn aws_machine_shape(
    info: &aws_sdk_ec2::types::InstanceTypeInfo,
    profile: &crate::config::RunnerProfile,
    exclusion_counts: &mut BTreeMap<String, usize>,
) -> Result<Option<AwsMachineShape>, CloudError> {
    let Some(machine_type) = info.instance_type().map(|kind| kind.as_str().to_owned()) else {
        *exclusion_counts
            .entry("instance type is missing its native name".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let Some(memory_mib) = info.memory_info().and_then(|memory| memory.size_in_mib()) else {
        *exclusion_counts
            .entry("machine type lacks memory capacity facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let memory_gib = Decimal::from(memory_mib)
        .checked_div(Decimal::from(1024u32))
        .ok_or_else(|| failure("inventory", "AWS memory conversion overflowed", vec![]))?;
    let Some(vcpu) = info
        .v_cpu_info()
        .and_then(|cpu| cpu.default_v_cpus())
        .and_then(|value| u32::try_from(value).ok())
    else {
        *exclusion_counts
            .entry("machine type lacks CPU capacity facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let architecture_value = info
        .processor_info()
        .map(|processor| processor.supported_architectures())
        .and_then(|values| values.first())
        .map(|value| value.as_str())
        .unwrap_or_default();
    let architecture = match architecture_value {
        "x86_64" => "amd64",
        "arm64" => "arm64",
        _ => architecture_value,
    };

    if architecture != profile.architecture
        || vcpu < profile.vcpu_min
        || vcpu > profile.vcpu_max
        || memory_gib < Decimal::from(profile.memory_gib_min)
        || memory_gib > Decimal::from(profile.memory_gib_max)
    {
        *exclusion_counts
            .entry("machine type does not meet requested architecture or compute bounds".to_owned())
            .or_default() += 1;
        return Ok(None);
    }

    let family = machine_type.split('.').next().map(str::to_owned);
    if profile.machine_families.get("aws").is_some_and(|families| {
        !family
            .as_ref()
            .is_some_and(|value| families.contains(value))
    }) {
        *exclusion_counts
            .entry("machine type is outside the requested families".to_owned())
            .or_default() += 1;
        return Ok(None);
    }

    let Some(gpu) = aws_gpu_shape(info)? else {
        *exclusion_counts
            .entry("machine type has incomplete accelerator facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let Some(local_scratch_gib) = aws_local_scratch(info) else {
        *exclusion_counts
            .entry("machine type has incomplete local-storage facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let ebs_info = info.ebs_info();
    if !ebs_info
        .and_then(|ebs| ebs.ebs_optimized_support())
        .is_some_and(|support| support.as_str() == "default")
    {
        *exclusion_counts
            .entry("machine type lacks supported EBS performance facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    }

    let Some(maximum_throughput_mbps) = ebs_info
        .and_then(|ebs| ebs.ebs_optimized_info())
        .and_then(|optimized| optimized.maximum_throughput_in_m_bps())
        .filter(|value| value.is_finite() && *value > 0.0)
    else {
        *exclusion_counts
            .entry("machine type lacks maximum EBS throughput facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let Some(ebs_iops) = ebs_info
        .and_then(|ebs| ebs.ebs_optimized_info())
        .and_then(|optimized| optimized.maximum_iops())
        .and_then(|iops| u32::try_from(iops).ok())
        .filter(|iops| *iops > 0)
    else {
        *exclusion_counts
            .entry("machine type lacks maximum EBS IOPS facts".to_owned())
            .or_default() += 1;
        return Ok(None);
    };

    let maximum_throughput_mbps = Decimal::from_str_exact(&maximum_throughput_mbps.to_string())
        .map_err(|_| {
            failure(
                "inventory",
                "AWS EBS throughput is not representable",
                vec![],
            )
        })?;
    let ebs_throughput_mib_s = maximum_throughput_mbps
        .checked_mul(Decimal::from(1_000_000u32))
        .and_then(|value| value.checked_div(Decimal::from(1_048_576u32)))
        .ok_or_else(|| {
            failure(
                "inventory",
                "AWS EBS throughput conversion overflowed",
                vec![],
            )
        })?;
    Ok(Some(AwsMachineShape {
        machine_type,
        family,
        vcpu,
        memory_gib,
        gpu,
        local_scratch_gib,
        ebs_iops,
        ebs_throughput_mib_s,
    }))
}

fn aws_gpu_shape(
    info: &aws_sdk_ec2::types::InstanceTypeInfo,
) -> Result<Option<AwsGpuShape>, CloudError> {
    let gpu = info.gpu_info();
    let devices = gpu.map(|gpu| gpu.gpus()).unwrap_or_default();
    let count = if devices.is_empty() {
        Some(0)
    } else {
        devices.iter().try_fold(0u32, |total, device| {
            let count = u32::try_from(device.count()?).ok()?;
            if count == 0 {
                return None;
            }

            total.checked_add(count)
        })
    };

    let Some(count) = count else { return Ok(None) };
    let memory_mib = gpu
        .and_then(|gpu| gpu.total_gpu_memory_in_mib())
        .map(i64::from)
        .or_else(|| {
            let devices = devices
                .iter()
                .map(|device| Some((device.count()?, device.memory_info()?.size_in_mib()?)))
                .collect::<Option<Vec<_>>>()?;
            devices
                .into_iter()
                .try_fold(0i64, |total, (count, memory)| {
                    total.checked_add(i64::from(count).checked_mul(i64::from(memory))?)
                })
        });
    let memory_gib = match (count, memory_mib) {
        (0, _) => None,
        (_, Some(memory_mib)) if memory_mib > 0 => Some(
            Decimal::from(memory_mib)
                .checked_div(Decimal::from(1024u32))
                .ok_or_else(|| {
                    failure("inventory", "AWS GPU memory conversion overflowed", vec![])
                })?,
        ),
        _ => return Ok(None),
    };

    Ok(Some(AwsGpuShape {
        vendor: devices
            .first()
            .and_then(|device| device.manufacturer())
            .map(str::to_owned),
        model: devices
            .first()
            .and_then(|device| device.name())
            .map(str::to_owned),
        count,
        memory_gib,
    }))
}

fn aws_local_scratch(info: &aws_sdk_ec2::types::InstanceTypeInfo) -> Option<Decimal> {
    match (
        info.instance_storage_supported(),
        info.instance_storage_info(),
    ) {
        (Some(false), _) => Some(Decimal::ZERO),
        (Some(true), Some(storage)) if storage.total_size_in_gb().is_some() => {
            Decimal::from(storage.total_size_in_gb().unwrap_or_default())
                .checked_mul(Decimal::from(1_000_000_000u64))
                .and_then(|bytes| bytes.checked_div(Decimal::from(1_073_741_824u64)))
        }

        _ => None,
    }
}

fn image_metadata_matches(image: &aws_sdk_ec2::types::Image, os: &str, architecture: &str) -> bool {
    let expected_architecture = if architecture == "amd64" {
        "x86_64"
    } else {
        architecture
    };

    let expected_platform = match os {
        "windows" => "Windows",
        "linux" => "Linux/UNIX",
        _ => return false,
    };

    image
        .state()
        .is_some_and(|state| state.as_str() == "available")
        && image
            .architecture()
            .is_some_and(|actual| actual.as_str() == expected_architecture)
        && image.platform_details() == Some(expected_platform)
        && image.usage_operation() == standard_usage_operation(os)
}

fn failure(kind: &str, message: &str, partial_resources: Vec<PlacementIdentity>) -> CloudError {
    failure_with_request_id(kind, message, partial_resources, None)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn failure_with_request_id(
    kind: &str,
    message: &str,
    partial_resources: Vec<PlacementIdentity>,
    provider_request_id: Option<String>,
) -> CloudError {
    CloudError::Failure(CloudFailure {
        kind: kind.to_owned(),
        message: message.to_owned(),
        partial_resources,
        provider_request_id,
    })
}

fn request_id<E>(error: &aws_sdk_ec2::error::SdkError<E>) -> Option<String> {
    error
        .raw_response()
        .and_then(|response| response.headers().get("x-amzn-requestid"))
        .or_else(|| {
            error
                .raw_response()
                .and_then(|response| response.headers().get("x-amz-request-id"))
        })
        .map(str::to_owned)
}

fn sdk_failure<E: aws_sdk_ec2::error::ProvideErrorMetadata>(
    error: &aws_sdk_ec2::error::SdkError<E>,
    message: &str,
    partial_resources: Vec<PlacementIdentity>,
) -> CloudError {
    let code = error
        .as_service_error()
        .and_then(|service| service.meta().code());
    let kind = match code {
        Some("UnauthorizedOperation" | "AuthFailure" | "AccessDeniedException") => {
            "authentication".to_owned()
        }

        Some("InvalidInstanceID.NotFound" | "InvalidAMIID.NotFound" | "InvalidVolume.NotFound") => {
            "not-found".to_owned()
        }

        Some("RequestLimitExceeded" | "Throttling" | "ThrottlingException") => {
            "throttled".to_owned()
        }

        Some(code)
            if !code.is_empty()
                && code.len() <= 80
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)) =>
        {
            format!("aws-{code}")
        }

        _ => "service".to_owned(),
    };

    failure_with_request_id(&kind, message, partial_resources, request_id(error))
}

fn standard_usage_operation(os: &str) -> Option<&'static str> {
    match os {
        "linux" => Some("RunInstances"),
        "windows" => Some("RunInstances:0002"),
        _ => None,
    }
}
