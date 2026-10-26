use outrider::config::AdmissionSettings;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(0);

pub(super) struct AdmissionState {
    pub(super) settings: AdmissionSettings,
    directory: PathBuf,
}

impl AdmissionState {
    pub(super) fn new() -> Self {
        let directory = std::env::current_dir()
            .unwrap_or_else(|error| panic!("working directory: {error}"))
            .join("target")
            .join(format!(
                "admission-window-{}-{}",
                std::process::id(),
                NEXT_WINDOW.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&directory)
            .unwrap_or_else(|error| panic!("create admission directory: {error}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .unwrap_or_else(|error| panic!("private directory: {error}"));
        }

        let directory = fs::canonicalize(directory)
            .unwrap_or_else(|error| panic!("canonical admission directory: {error}"));
        Self {
            settings: AdmissionSettings {
                state_file: directory.join("state.json"),
                max_window_seconds: 1800,
            },
            directory,
        }
    }

    pub(super) fn write(&self, opened: u64, expires: u64) {
        self.write_raw(&format!(
            "{{\"version\":1,\"opened_at_unix\":{opened},\"expires_at_unix\":{expires}}}"
        ));
    }

    pub(super) fn write_raw(&self, content: &str) {
        fs::write(&self.settings.state_file, content)
            .unwrap_or_else(|error| panic!("write admission state: {error}"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.settings.state_file, fs::Permissions::from_mode(0o600))
                .unwrap_or_else(|error| panic!("private state: {error}"));
        }
    }

    pub(super) fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|error| panic!("Unix time: {error}"))
            .as_secs()
    }
}

impl Drop for AdmissionState {
    fn drop(&mut self) {
        let _cleanup = fs::remove_dir_all(&self.directory);
    }
}
