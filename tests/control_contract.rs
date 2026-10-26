#[path = "fixtures/native_central.rs"]
mod native_central;
use native_central::{load_fixture_central, serialize_native_central};

#[path = "control_contract/daemon_scenario.rs"]
mod daemon_scenario;
#[path = "control_contract/process.rs"]
mod process;
#[path = "control_contract/provider.rs"]
mod provider;
#[path = "control_contract/provider_wire.rs"]
mod provider_wire;
#[path = "control_contract/restart.rs"]
mod restart;
#[path = "control_contract/tombstones.rs"]
mod tombstones;
