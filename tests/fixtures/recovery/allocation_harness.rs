use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use outrider::{
    allocation::{
        AllocationError, AllocationService, ConnectedCatalog, catalog_configuration_fingerprint,
    },
    cloud::aws::AwsBackend,
    config::ConfigPatch,
    garm::{BootstrapInstance, ProviderRequest, RunnerApplicationDownload, UserDataOptions},
    github::{OwnerRef, RepositoryRef, SnapshotDocument},
    journal::{Journal, PoolSnapshot},
    policy::{MissingDataBehavior, Policy, PolicyDefinition},
    providers::{ProcessLimits, ProviderRegistry, PurchasingCapability},
};
use serde_json::{Value, json};

#[path = "../system/aws_sdk_fixture.rs"]
pub(super) mod aws_sdk_fixture;
pub(super) use aws_sdk_fixture::{JournalFixture, allocation_binding, allocation_central};
use aws_sdk_fixture::{SubnetMetadataBehavior, aws_server};

#[path = "allocation_harness/aws_catalog.rs"]
mod aws_catalog;
pub(super) use aws_catalog::*;
#[path = "allocation_harness/native_cleanup.rs"]
mod native_cleanup;
pub use native_cleanup::verified_interruption_only_case;
pub(super) use native_cleanup::*;
#[path = "allocation_harness/spot_interruption.rs"]
mod spot_interruption;
pub(super) use spot_interruption::*;
pub use spot_interruption::{
    first_delete_retry_excludes_interruption, interruption_poll_progress_is_independent,
    observation_provenance_case, spot_conflicting_source_case, spot_observation_case,
    wait_for_test_time,
};
