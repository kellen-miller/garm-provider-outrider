use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use azure_core::{
    credentials::{AccessToken, TokenCredential, TokenRequestOptions},
    time::OffsetDateTime,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use outrider::{
    allocation::{
        AllocationError, AllocationService, ConnectedCatalog, canonical_native_account_scope,
        catalog_configuration_fingerprint,
    },
    cloud::azure::AzureBackend,
    garm::ProviderRequest,
    journal::{ChildAttemptRecord, Journal, PoolSnapshot},
    providers::{ProcessLimits, ProviderRegistry},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;

#[path = "fixtures/recovery/allocation_harness.rs"]
pub mod allocation_harness;
use allocation_harness::*;

#[path = "allocation_contract/azure_cleanup.rs"]
mod azure_cleanup;
#[path = "allocation_contract/native_ownership.rs"]
mod native_ownership;
#[path = "allocation_contract/offer_revalidation.rs"]
mod offer_revalidation;
#[path = "allocation_contract/pool_preparation.rs"]
mod pool_preparation;
#[path = "allocation_contract/snapshot_admission.rs"]
mod snapshot_admission;
#[path = "allocation_contract/uncertain_create.rs"]
mod uncertain_create;
