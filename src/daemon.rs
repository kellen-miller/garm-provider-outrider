use std::{collections::BTreeMap, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};

use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::Sha256;

use crate::{
    allocation::{
        AllocationError, AllocationService, ExplainRequest, ExplanationReport, InventoryReport,
    },
    config::{CentralConfig, ControlSettings, IngressSettings},
    garm::{ProviderRequest, ProviderResponse},
    github::{IngressError, IngressService},
    journal::Journal,
    recovery::RecoveryService,
};

type AuthMac = Hmac<Sha256>;

#[derive(Clone)]
struct DaemonState {
    auth_token: Arc<Vec<u8>>,
    allocation: Arc<AllocationService>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct SafeError {
    error: &'static str,
}

/// Start the loopback provider daemon and serve health and provider requests.
///
/// # Errors
///
/// Returns an error when configuration, journal setup, socket binding, or
/// server startup fails.
pub async fn serve(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
    let mut source = CentralConfig::from_toml(&std::fs::read_to_string(config_path)?)?;
    let authentication = source.authentication.take();
    let webhook_address = source
        .webhook_listen_address
        .take()
        .map(|address| address.parse::<SocketAddr>())
        .transpose()
        .map_err(|_| "webhook_listen_address must be a socket address")?;
    let mut config = source.load_native_configuration(&environment)?;
    let settings = config
        .control
        .as_ref()
        .ok_or("control settings are required")?;
    let address = validate_settings(settings)?;
    let token = std::fs::read(&settings.auth_token_file)?;
    if token.trim_ascii().is_empty() {
        return Err("internal auth token file is empty".into());
    }

    let renewal = if let Some(authentication) = authentication {
        let ingress_settings = config
            .ingress
            .as_ref()
            .ok_or("ingress settings are required")?;
        let renewal =
            crate::authentication::TokenRenewal::connect(authentication, ingress_settings).await?;
        let resolved = crate::setup::adoption::resolve_identities(ingress_settings).await?;
        config.ingress = Some(resolved);
        Some(renewal)
    } else {
        None
    };

    let journal = Journal::open(Path::new(&settings.database_path))?;
    let journal = Arc::new(journal);
    let allocation =
        Arc::new(AllocationService::connect(config.clone(), journal.clone(), environment).await?);
    let ingress = Arc::new(IngressService::new(config, journal.clone())?);
    let recovery = Arc::new(RecoveryService::new(
        allocation.clone(),
        ingress.clone(),
        journal,
    ));
    recovery.startup()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let public_listener = if let Some(address) = webhook_address {
        Some(tokio::net::TcpListener::bind(address).await?)
    } else {
        None
    };
    let public_app = build_webhook_router(ingress.clone());
    let app = build_router(token.trim_ascii().to_vec(), allocation, ingress)?;
    if let Some(mut renewal) = renewal {
        tokio::spawn(async move {
            loop {
                let delay = match renewal.refresh_due().await {
                    Ok(delay) => delay,
                    Err(error) => {
                        eprintln!("outrider authentication: {error}");
                        renewal.report().next_refresh_in
                    }
                };
                tokio::time::sleep(delay).await;
            }
        });
    }

    tokio::spawn(async move {
        loop {
            if recovery.iteration(32).await.is_err() {
                eprintln!("outrider recovery iteration has unresolved work");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    if let Some(public_listener) = public_listener {
        tokio::try_join!(async { axum::serve(listener, app).await }, async {
            axum::serve(public_listener, public_app).await
        })?;
    } else {
        axum::serve(listener, app).await?;
    }

    Ok(())
}

/// Send a bounded authenticated explanation request to the configured loopback daemon.
///
/// # Errors
/// Returns an error when control settings, token, timeout, transport, response
/// status, response size, or report JSON is invalid.
pub async fn explain_remote(
    settings: &ControlSettings,
    ingress: &IngressSettings,
    request: ExplainRequest,
) -> Result<ExplanationReport, Box<dyn std::error::Error>> {
    control_request(settings, ingress, "explain", &request, 1_000_000).await
}

/// Collect native VM/disk/address metadata through the authenticated loopback daemon.
///
/// # Errors
/// Returns an error if authentication, any native collection, or bounded transport fails.
pub async fn inventory_remote(
    settings: &ControlSettings,
    ingress: &IngressSettings,
) -> Result<InventoryReport, Box<dyn std::error::Error>> {
    control_request(settings, ingress, "inventory", &(), 32 * 1024 * 1024).await
}

async fn control_request<Request: serde::Serialize, Report: serde::de::DeserializeOwned>(
    settings: &ControlSettings,
    ingress: &IngressSettings,
    operation: &str,
    request: &Request,
    max_bytes: usize,
) -> Result<Report, Box<dyn std::error::Error>> {
    let address = validate_settings(settings)?;
    let token = std::fs::read(&settings.auth_token_file)?;
    if token.trim_ascii().is_empty() {
        return Err("internal auth token file is empty".into());
    }
    let bootstrap_seconds = u64::from(ingress.bootstrap_timeout_minutes)
        .checked_mul(60)
        .ok_or("analysis timeout is invalid")?;
    let budget = ingress
        .allocation_timeout_seconds
        .min(bootstrap_seconds)
        .checked_sub(ingress.timeout_margin_seconds)
        .filter(|budget| *budget > 0)
        .ok_or("analysis timeout is invalid")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(budget))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;
    let response = client
        .post(format!("http://{address}/v1/{operation}"))
        .bearer_auth(String::from_utf8_lossy(token.trim_ascii()).to_string())
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err("daemon rejected the control request".into());
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err("daemon control response exceeds the size limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Build the production provider and webhook HTTP router from required services.
///
/// # Errors
/// Returns an error when the internal bearer token is empty.
pub fn build_router(
    auth_token: Vec<u8>,
    allocation: Arc<AllocationService>,
    ingress: Arc<IngressService>,
) -> Result<Router, &'static str> {
    if auth_token.is_empty() {
        return Err("internal auth token is required");
    }
    let state = DaemonState {
        auth_token: Arc::new(auth_token),
        allocation,
    };
    Ok(Router::new()
        .route("/v1/provider", post(provider))
        .route("/v1/explain", post(explain))
        .route("/v1/inventory", post(inventory))
        .with_state(state)
        .merge(build_webhook_router(ingress)))
}

/// Public ingress contains no provider/control routes or bearer credentials.
pub fn build_webhook_router(ingress: Arc<IngressService>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/webhooks/github", post(github_webhook))
        .with_state(ingress)
}

fn validate_settings(settings: &ControlSettings) -> Result<SocketAddr, &'static str> {
    if settings.listen_address.is_empty()
        || settings.database_path.is_empty()
        || settings.auth_token_file.is_empty()
    {
        return Err("listen_address, database_path and auth_token_file are required");
    }

    let address = settings
        .listen_address
        .parse::<SocketAddr>()
        .map_err(|_| "listen_address must be a socket address")?;
    if !address.ip().is_loopback() {
        return Err("daemon bearer transport must bind to a loopback address");
    }

    if settings.provider_timeout_seconds == 0 {
        return Err("provider_timeout_seconds must be positive");
    }

    Ok(address)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ready" })
}

async fn inventory(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> Result<Json<InventoryReport>, (StatusCode, Json<SafeError>)> {
    if !authorized(&headers, &state.auth_token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(SafeError {
                error: "unauthorized",
            }),
        ));
    }

    state.allocation.inventory().await.map(Json).map_err(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(SafeError {
                error: "native inventory is incomplete or unavailable",
            }),
        )
    })
}

async fn provider(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ProviderRequest>,
) -> (StatusCode, Json<ProviderResponse>) {
    if !authorized(&headers, &state.auth_token) {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }

    if let Err(message) = validate_request(&request) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }

    let allocation = state.allocation.clone();
    let execution = tokio::spawn(async move { allocation.handle(request).await });
    match execution.await {
        Ok(Ok(result)) => (
            StatusCode::OK,
            Json(ProviderResponse {
                result,
                error: None,
                not_found: false,
            }),
        ),
        Ok(Err(AllocationError::NotFound)) => (
            StatusCode::NOT_FOUND,
            Json(ProviderResponse {
                result: None,
                error: Some("allocation is absent".to_owned()),
                not_found: true,
            }),
        ),
        Ok(Err(AllocationError::DeletedName)) => error_response(
            StatusCode::CONFLICT,
            allocation_error_label(AllocationError::DeletedName),
        ),
        Ok(Err(error)) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            allocation_error_label(error),
        ),
        Err(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "allocation execution remains unresolved",
        ),
    }
}

async fn explain(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<ExplainRequest>,
) -> Result<Json<ExplanationReport>, (StatusCode, Json<SafeError>)> {
    if !authorized(&headers, &state.auth_token) {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(SafeError {
                error: "unauthorized",
            }),
        ));
    }
    if request
        .profile
        .as_ref()
        .is_some_and(|value| value.trim().is_empty())
        || request
            .snapshot_id
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
        || (request.profile.is_some() && request.snapshot_id.is_some())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(SafeError {
                error: "invalid explanation selector",
            }),
        ));
    }
    match state.allocation.explain(request).await {
        Ok(report) => Ok(Json(report)),
        Err(AllocationError::Snapshot) => Err((
            StatusCode::NOT_FOUND,
            Json(SafeError {
                error: "snapshot is unavailable or ineligible",
            }),
        )),
        Err(AllocationError::Configuration) => Err((
            StatusCode::BAD_REQUEST,
            Json(SafeError {
                error: "profile or configuration is invalid",
            }),
        )),
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(SafeError {
                error: "explanation analysis is unavailable",
            }),
        )),
    }
}

async fn github_webhook(
    State(ingress): State<Arc<IngressService>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    let original_headers = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect::<BTreeMap<_, _>>();
    match ingress.intake(
        header("x-github-event"),
        header("x-github-delivery"),
        header("x-hub-signature-256"),
        original_headers,
        &body,
    ) {
        Ok(_) => StatusCode::ACCEPTED,
        Err(IngressError::Configuration | IngressError::Journal) => StatusCode::SERVICE_UNAVAILABLE,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

fn allocation_error_label(error: AllocationError) -> &'static str {
    match error {
        AllocationError::Configuration => "allocation configuration is unavailable",
        AllocationError::Snapshot => "allocation snapshot is unavailable or stale",
        AllocationError::Conflict => "allocation identity conflicts with an existing request",
        AllocationError::DeletedName => "runner name was deleted before allocation reservation",
        AllocationError::NotFound => "allocation is definitely absent",
        AllocationError::AdmissionClosed => "administrator allocation admission is closed",
        AllocationError::Limit => "allocation concurrency limit reached",
        AllocationError::Catalog => "allocation catalog is unavailable",
        AllocationError::NoCandidate => "no eligible provider candidate",
        AllocationError::Ambiguous => "provider outcome remains unresolved",
        AllocationError::Provider => "provider lifecycle operation failed",
        AllocationError::Journal => "allocation journal operation failed",
    }
}

fn error_response(
    status: StatusCode,
    error: impl Into<String>,
) -> (StatusCode, Json<ProviderResponse>) {
    (
        status,
        Json(ProviderResponse {
            result: None,
            error: Some(error.into()),
            not_found: false,
        }),
    )
}

fn authorized(headers: &HeaderMap, token: &[u8]) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    let Some(presented) = value.strip_prefix("Bearer ") else {
        return false;
    };

    let Ok(mut mac) = AuthMac::new_from_slice(token) else {
        return false;
    };

    mac.update(presented.as_bytes());
    let Ok(mut expected) = AuthMac::new_from_slice(token) else {
        return false;
    };

    expected.update(token);
    mac.verify_slice(&expected.finalize().into_bytes()).is_ok()
}

fn validate_request(request: &ProviderRequest) -> Result<(), &'static str> {
    if request.controller_id.trim().is_empty() {
        return Err("GARM_CONTROLLER_ID is required");
    }

    if !matches!(request.interface_version.as_str(), "v0.1.0" | "v0.1.1") {
        return Err("unsupported GARM interface version");
    }

    match request.command.as_str() {
        "CreateInstance"
            if request
                .bootstrap
                .as_ref()
                .is_none_or(|bootstrap| bootstrap.name.trim().is_empty()) =>
        {
            Err("CreateInstance requires a named BootstrapInstance")
        }

        "CreateInstance" if request.pool_id.as_deref().is_none_or(str::is_empty) => {
            Err("CreateInstance requires GARM_POOL_ID")
        }

        "CreateInstance" | "GetInstance" | "DeleteInstance" | "StartInstance" | "StopInstance"
            if request.instance_id.as_deref().is_none_or(str::is_empty)
                && request
                    .bootstrap
                    .as_ref()
                    .is_none_or(|bootstrap| bootstrap.name.is_empty()) =>
        {
            Err("instance operation requires GARM_INSTANCE_ID or bootstrap name")
        }

        "DeleteInstance" | "GetInstance" | "StartInstance" | "StopInstance"
            if request.interface_version == "v0.1.1"
                && request.pool_id.as_deref().is_none_or(str::is_empty) =>
        {
            Err("GARM v0.1.1 instance operation requires GARM_POOL_ID")
        }

        "ListInstances" if request.pool_id.as_deref().is_none_or(str::is_empty) => {
            Err("ListInstances requires GARM_POOL_ID")
        }

        "ValidatePoolInfo"
            if request.interface_version != "v0.1.1"
                || request
                    .pool_extra_specs
                    .as_ref()
                    .and_then(Value::as_object)
                    .and_then(|extra_specs| extra_specs.get("snapshot_id"))
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty) =>
        {
            Err("ValidatePoolInfo requires a nonempty pool snapshot_id")
        }

        "ListInstances"
        | "RemoveAllInstances"
        | "GetVersion"
        | "GetSupportedInterfaceVersions"
        | "ValidatePoolInfo"
        | "GetConfigJSONSchema"
        | "GetExtraSpecsJSONSchema" => Ok(()),
        "CreateInstance" | "GetInstance" | "DeleteInstance" | "StartInstance" | "StopInstance" => {
            Ok(())
        }

        _ => Err("unsupported GARM_COMMAND"),
    }
}
