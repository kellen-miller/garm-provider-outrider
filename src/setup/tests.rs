//! Setup contracts exercise real protected files and read-only API adoption.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

mod identities;
mod protected_configuration;
mod registration;
mod source_boundaries;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct ProtectedDirectory {
    path: PathBuf,
}

impl ProtectedDirectory {
    fn new() -> Self {
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "setup-contract-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self { path }
    }

    fn file(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProtectedDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
