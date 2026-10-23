use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use outrider::{
    cloud::PlacementIdentity,
    journal::{
        AllocationAdmission, ChildAttemptRecord, CreateHistoryRecord, Journal, OutboxMessage,
        PoolSnapshot, RunningHistoryRecord, WebhookDelivery,
    },
};

#[path = "fixtures/recovery/allocation_harness.rs"]
pub mod allocation_harness;

#[path = "recovery_contract/startup_cleanup.rs"]
mod startup_cleanup;
use startup_cleanup::*;
#[path = "recovery_contract/create_history.rs"]
mod create_history;
#[path = "recovery_contract/history_samples.rs"]
mod history_samples;
#[path = "recovery_contract/history_staleness.rs"]
mod history_staleness;
#[path = "recovery_contract/interruption_evidence.rs"]
mod interruption_evidence;
#[path = "recovery_contract/rerun_http.rs"]
mod rerun_http;
#[path = "recovery_contract/rerun_state.rs"]
mod rerun_state;
#[path = "recovery_contract/retention_batches.rs"]
mod retention_batches;
#[path = "recovery_contract/retention_history.rs"]
mod retention_history;
use retention_batches::*;
