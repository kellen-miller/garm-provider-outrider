use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs, io,
    net::TcpListener,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Request, StatusCode},
    response::Response,
    routing::any,
};
use azure_core::{
    credentials::{AccessToken, TokenCredential, TokenRequestOptions},
    time::OffsetDateTime,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use google_cloud_auth::credentials::{
    AccessToken as GcpAccessToken, AccessTokenCredentials, AccessTokenCredentialsProvider,
    CacheableResource, CredentialsProvider, EntityTag,
};
use hmac::{Hmac, KeyInit, Mac};
use hyper::http::{Extensions, HeaderMap, HeaderValue};
use outrider::{
    allocation::{AllocationService, ConnectedCatalog, catalog_configuration_fingerprint},
    cloud::{CatalogQuery, CloudCatalog, aws::AwsBackend, azure::AzureBackend, gcp::GcpBackend},
    config::{
        AzureCatalogBinding, ConfigPatch, EffectivePrice, EffectivePriceComponent,
        GcpCatalogBinding, IngressRepository, RerunSettings,
    },
    github::IngressService,
    providers::{ProcessLimits, ProviderBinding, ProviderRegistry, PurchasingCapability},
    recovery::RecoveryService,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{net::TcpListener as TokioTcpListener, task::JoinHandle};

#[path = "fixtures/system/aws_sdk_fixture.rs"]
mod aws_sdk_fixture;
use aws_sdk_fixture::{
    AwsServer, JournalFixture, NativeOwnership, allocation_binding, allocation_central, aws_server,
};

#[path = "system_contract/github_events.rs"]
mod github_events;
use github_events::*;
#[path = "system_contract/cloud_http.rs"]
mod cloud_http;
use cloud_http::*;
#[path = "system_contract/system_aws.rs"]
mod system_aws;
use system_aws::*;
#[path = "system_contract/cloud_quotes.rs"]
mod cloud_quotes;
use cloud_quotes::*;
#[path = "system_contract/cross_cloud_catalogs.rs"]
mod cross_cloud_catalogs;
#[path = "system_contract/cross_cloud_scenario.rs"]
mod cross_cloud_scenario;
use cross_cloud_catalogs::*;
use cross_cloud_scenario::CrossCloudScenario;
#[path = "system_contract/daemon_scenario.rs"]
mod daemon_scenario;
use daemon_scenario::*;
#[path = "system_contract/authenticated_explain.rs"]
mod authenticated_explain;
use authenticated_explain::*;
#[path = "system_contract/authenticated_inventory.rs"]
mod authenticated_inventory;
#[path = "system_contract/cross_cloud_cleanup.rs"]
mod cross_cloud_cleanup;
#[path = "system_contract/daemon_restart.rs"]
mod daemon_restart;
#[path = "system_contract/price_provenance.rs"]
mod price_provenance;
use cross_cloud_cleanup::*;
#[path = "system_contract/explain_deadlines.rs"]
mod explain_deadlines;
