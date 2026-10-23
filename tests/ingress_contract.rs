use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response, StatusCode, body::Incoming, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use outrider::{
    cloud::PlacementIdentity,
    config::{
        AdministratorLimits, AwsCatalogBinding, AwsImageBinding, ConfigPatch, IngressRepository,
        IngressSettings, RerunSettings, RunnerProfile,
    },
    github::{GitHubClient, IngressService, OwnerRef, RepositoryRef, WorkflowJobEvent},
    journal::{
        AllocationAdmission, ChildAttemptRecord, InterruptionRecord, JobObservationRecord,
        JobRerunReservation, Journal, OutboxMessage, PoolSnapshot, WebhookDelivery,
    },
    label::RoutingRequest,
    pools::{GarmClient, PoolError, RepositoryPoolRequest},
};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::{net::TcpListener, task::JoinHandle};

#[path = "ingress_contract/github_events.rs"]
mod github_events;
use github_events::*;
#[path = "ingress_contract/signed_admission.rs"]
mod signed_admission;
use signed_admission::*;
#[path = "ingress_contract/job_provenance.rs"]
mod job_provenance;
#[path = "ingress_contract/job_terminal_barriers.rs"]
mod job_terminal_barriers;
#[path = "ingress_contract/pool_create_fencing.rs"]
mod pool_create_fencing;
#[path = "ingress_contract/pool_lifecycle.rs"]
mod pool_lifecycle;
#[path = "ingress_contract/rerun_state.rs"]
mod rerun_state;
#[path = "ingress_contract/terminal_intake.rs"]
mod terminal_intake;
