//! A real child process for provider-protocol, recovery, and daemon tests.
use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use outrider_xtask::provider_fixture::{Configuration, Scenario};
use serde_json::{Value, json};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn variable(name: &str) -> Option<String> {
    env::var(name).ok()
}

fn stdin_bytes() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn append(path: &Path, value: &Value) -> Result<()> {
    let mut record = serde_json::to_vec(value)?;
    record.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&record)?;
    Ok(())
}

fn monotonic() -> Result<f64> {
    #[cfg(unix)]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes one initialized, valid timespec pointer.
        let status = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
        ensure!(status == 0, "read monotonic clock");
        Ok(Duration::new(u64::try_from(time.tv_sec)?, u32::try_from(time.tv_nsec)?).as_secs_f64())
    }

    #[cfg(not(unix))]
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64())
}

fn wait_barrier(path: &Path, interval: Duration) -> Result<()> {
    fs::write(path, "ready")?;
    let release = PathBuf::from(format!("{}.release", path.display()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !release.exists() {
        ensure!(Instant::now() < deadline, "bounded fixture barrier expired");
        thread::sleep(interval);
    }

    Ok(())
}

fn instance(locator: &str, name: &str, fault: Option<&[u8]>) -> Value {
    let mut instance = json!({"provider_id":locator,"name":name,"os_type":"linux","os_name":"Linux","os_version":"test","os_arch":"x86_64","addresses":[],"status":"running"});
    if let Some(fault) = fault {
        instance["provider_fault"] = json!(STANDARD.encode(fault));
    }

    instance
}

fn protocol(command: &str, mode: &str, configuration: &Configuration) -> Result<()> {
    if let Some(path) = variable("OUTRIDER_LAUNCH_LOG") {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(format!("{command}\n").as_bytes())?;
    }

    if let Some(path) = variable("OUTRIDER_LIFECYCLE_LOG") {
        let input = if command == "DeleteInstance"
            && ["create_failure", "barrier_create"].contains(&mode)
        {
            Some(STANDARD.encode(stdin_bytes()?))
        } else {
            None
        };
        append(
            Path::new(&path),
            &json!({"command":command,"controller_id":variable("GARM_CONTROLLER_ID"),"pool_id":variable("GARM_POOL_ID"),"instance_id":variable("GARM_INSTANCE_ID"),"at_monotonic":monotonic()?,"stdin":input}),
        )?;
    }

    if command == "GetVersion" {
        if mode == "admission_version_barrier" {
            let marker = variable("OUTRIDER_MARKER")
                .ok_or_else(|| anyhow::anyhow!("admission marker required"))?;
            wait_barrier(Path::new(&marker), Duration::from_millis(10))?;
        }

        print!(
            "{}",
            if mode == "version_mismatch" {
                "fixture-other-build"
            } else {
                "fixture-build-1"
            }
        );
        return Ok(());
    }

    if mode == "no_stdin_reader" {
        thread::sleep(Duration::from_secs(30));
    }

    let raw = if command == "CreateInstance" {
        stdin_bytes()?
    } else {
        Vec::new()
    };
    if command == "CreateInstance" && mode == "record" {
        fs::write(
            env::var("OUTRIDER_RECORD")?,
            serde_json::to_vec(
                &json!({"raw":STANDARD.encode(&raw),"controller":variable("GARM_CONTROLLER_ID"),"pool":variable("GARM_POOL_ID"),"instance":variable("GARM_INSTANCE_ID"),"interface":variable("GARM_INTERFACE_VERSION"),"config":variable("GARM_PROVIDER_CONFIG_FILE"),"safe":variable("SAFE_CREDENTIAL"),"ambient":variable("AMBIENT_CREDENTIAL"),"ambient_garm":variable("GARM_AMBIENT_SECRET")}),
            )?,
        )?;
    }

    let request: Value = if raw.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&raw)?
    };
    if command == "CreateInstance"
        && (mode == "create_failure"
            || (mode == "mixed_create" && request["name"] == "unknown-create"))
    {
        eprint!("opaque Create failure");
        std::process::exit(1);
    }

    if command == "CreateInstance" && mode == "barrier_create" {
        let barrier = env::var("OUTRIDER_CREATE_BARRIER")?;
        fs::write(format!("{barrier}.ready"), "ready")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !PathBuf::from(format!("{barrier}.release")).exists() {
            ensure!(Instant::now() < deadline, "bounded Create barrier expired");
            thread::sleep(Duration::from_millis(10));
        }
    }

    if ["exit30", "exit31", "exit1", "null_nonzero"].contains(&mode) {
        eprintln!("capacity secret=do-not-leak");
        if mode == "null_nonzero" {
            print!("null");
        }

        std::process::exit(match mode {
            "exit1" => 1,
            "exit31" => 31,
            _ => 30,
        });
    }

    match mode {
        "malformed" => {
            print!("{{bad json");
            return Ok(());
        }
        "stdout_flood" => {
            print!("{}", "x".repeat(200_000));
            return Ok(());
        }
        "stderr_flood" => {
            eprint!("{}", "x".repeat(200_000));
            return Ok(());
        }
        "descendant" => {
            let child = Command::new(env::current_exe()?)
                .arg("descendant-marker")
                .arg(env::var("OUTRIDER_MARKER")?)
                .stdin(Stdio::null())
                .spawn()?;
            let path = env::var("OUTRIDER_PID_FILE")?;
            fs::write(
                format!("{path}.tmp"),
                format!("{}\n{}\n", std::process::id(), child.id()),
            )?;
            fs::rename(format!("{path}.tmp"), &path)?;
            thread::sleep(Duration::from_secs(30));
        }
        _ => {}
    }

    if command == "DeleteInstance" && mode == "multiple_slow_delete" {
        thread::sleep(Duration::from_secs(30));
    }

    let locator = variable("OUTRIDER_NATIVE_LOCATOR")
        .or_else(|| configuration.native_locator.clone())
        .unwrap_or_else(|| "i-synthetic-001".to_owned());
    let locator = if command == "CreateInstance"
        && mode == "multiple_slow_delete"
        && request["name"] == "runner-second"
    {
        "i-synthetic-002"
    } else {
        &locator
    };
    let result = instance(locator, "runner-synthetic", Some(b"secret child details"));
    if mode == "extra" {
        print!("{result} extra");
        return Ok(());
    }

    match command {
        "CreateInstance" | "GetInstance" => print!("{result}"),
        "ListInstances" => {
            if mode == "null" {
                print!("null");
            } else {
                print!(
                    "{}",
                    if mode == "omit_list" {
                        json!([])
                    } else {
                        json!([result])
                    }
                );
            }
        }
        _ => {
            if mode == "empty_output" {
                print!("unexpected");
            }
        }
    }

    Ok(())
}

fn recovery(command: &str, mode: &str) -> Result<()> {
    if let Some(path) = variable("OUTRIDER_LIFECYCLE_LOG") {
        append(
            Path::new(&path),
            &json!({"command":command,"instance_id":variable("GARM_INSTANCE_ID"),"at_monotonic":monotonic()?}),
        )?;
    }

    match command {
        "GetVersion" => print!("fixture-build-1"),
        "CreateInstance" => {
            let request: Value = serde_json::from_slice(&stdin_bytes()?)?;
            let locator = match request["name"].as_str() {
                Some("runner-poison") => "i-synthetic-003",
                Some("runner-second") => "i-synthetic-002",
                _ => "i-synthetic-001",
            };
            print!(
                "{}",
                instance(
                    locator,
                    request["name"].as_str().unwrap_or_default(),
                    Some(b"fixture detail")
                )
            );
        }
        "DeleteInstance" => {
            if mode == "slow_first_delete"
                && variable("GARM_INSTANCE_ID").as_deref() == Some("i-synthetic-001")
            {
                thread::sleep(Duration::from_secs(30));
            }

            if mode == "slow_first_delete"
                && variable("GARM_INSTANCE_ID").as_deref() == Some("i-synthetic-002")
            {
                wait_barrier(
                    Path::new(&format!(
                        "{}.second-ready",
                        env::var("OUTRIDER_LIFECYCLE_LOG")?
                    )),
                    Duration::from_millis(5),
                )?;
            }
        }
        "ListInstances" => print!("[]"),
        _ => {}
    }

    Ok(())
}

fn system(command: &str, configuration: &Configuration) -> Result<()> {
    if command == "GetVersion" {
        print!("fixture-build-1");
        return Ok(());
    }

    let log = configuration
        .lifecycle_log
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("lifecycle log required"))?;
    append(
        log,
        &json!({"command":command,"controller_id":variable("GARM_CONTROLLER_ID"),"pool_id":variable("GARM_POOL_ID").unwrap_or_default(),"instance_id":variable("GARM_INSTANCE_ID").unwrap_or_default()}),
    )?;
    let state = PathBuf::from(format!("{}.instances.json", log.display()));
    let mut rows: Vec<Value> = match fs::read(&state) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let pool = variable("GARM_POOL_ID").unwrap_or_default();
    let id = variable("GARM_INSTANCE_ID").unwrap_or_default();
    match command {
        "CreateInstance" => {
            let request: Value = serde_json::from_slice(&stdin_bytes()?)?;
            let locator = variable("OUTRIDER_NATIVE_LOCATOR")
                .or_else(|| configuration.native_locator.clone())
                .unwrap_or_else(|| "i-synthetic-001".to_owned());
            let instance = instance(&locator, request["name"].as_str().unwrap_or_default(), None);
            rows.push(json!({"pool_id":pool,"instance":instance}));
            print!("{instance}");
        }
        "ListInstances" => print!(
            "{}",
            json!(
                rows.iter()
                    .filter(|r| r["pool_id"] == pool)
                    .map(|r| r["instance"].clone())
                    .collect::<Vec<_>>()
            )
        ),
        "GetInstance" => print!(
            "{}",
            rows.iter()
                .find(|r| r["pool_id"] == pool && r["instance"]["provider_id"] == id)
                .ok_or_else(|| anyhow::anyhow!("missing instance"))?["instance"]
        ),
        "DeleteInstance" => {
            rows.retain(|r| !(r["pool_id"] == pool && r["instance"]["provider_id"] == id))
        }
        _ => anyhow::bail!("unknown fixture command"),
    }

    if ["CreateInstance", "DeleteInstance"].contains(&command) {
        let temporary = state.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec(&rows)?)?;
        fs::rename(temporary, state)?;
    }

    Ok(())
}

fn main() -> Result<()> {
    let arguments: Vec<String> = env::args().collect();
    if arguments.get(1).is_some_and(|v| v == "descendant-marker") {
        thread::sleep(Duration::from_secs(1));
        fs::write(&arguments[2], "alive")?;
        return Ok(());
    }

    let config = env::current_exe()?.with_extension("json");
    let configuration: Configuration = if config.is_file() {
        serde_json::from_slice(&fs::read(config)?)?
    } else {
        Configuration::default()
    };
    let command = variable("GARM_COMMAND").unwrap_or_default();
    let mode = variable("OUTRIDER_FIXTURE_MODE").unwrap_or_else(|| "normal".to_owned());
    match configuration.scenario {
        Scenario::Protocol => protocol(&command, &mode, &configuration),
        Scenario::Recovery => recovery(&command, &mode),
        Scenario::System => system(&command, &configuration),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_lifecycle_records_remain_complete_json_lines() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("lifecycle.jsonl");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for writer in 0..8 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || -> Result<()> {
                barrier.wait();
                for sequence in 0..32 {
                    append(&path, &json!({"writer":writer,"sequence":sequence,"command":"DeleteInstance","controller_id":"controller-1","pool_id":"pool-1","instance_id":"i-fixture","at_monotonic":monotonic()?}))?;
                }

                Ok(())
            }));
        }

        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("record writer panicked"))??;
        }

        let text = fs::read_to_string(path)?;
        assert_eq!(text.lines().count(), 256);
        for line in text.lines() {
            serde_json::from_str::<Value>(line)?;
        }

        Ok(())
    }
}
