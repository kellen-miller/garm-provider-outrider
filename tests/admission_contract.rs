#[path = "fixtures/admission_state.rs"]
mod admission_state;
use admission_state::AdmissionState;
use outrider::{ConfigPatch, admission::allocation_is_open};
use std::fs;

#[test]
fn configured_window_is_persistent_bounded_and_fail_closed() {
    assert!(allocation_is_open(None));
    let state = AdmissionState::new();
    assert!(!allocation_is_open(Some(&state.settings)));
    let now = AdmissionState::now();
    state.write(now - 1, now + 1799);
    assert!(allocation_is_open(Some(&state.settings)));
    let reopened = state.settings.clone();
    assert!(allocation_is_open(Some(&reopened)));
    state.write(now - 1800, now);
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write(now + 1, now + 60);
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write(now, now + 1801);
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write(0, 0);
    assert!(!allocation_is_open(Some(&state.settings)));
    state
        .write_raw("{\"version\":2,\"opened_at_unix\":0,\"expires_at_unix\":18446744073709551615}");
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write_raw("{\"version\":1,\"opened_at_unix\":1.2,\"expires_at_unix\":9}");
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write_raw("{\"version\":1,\"opened_at_unix\":1,\"expires_at_unix\":9,\"extra\":true}");
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write_raw(&"x".repeat(4097));
    assert!(!allocation_is_open(Some(&state.settings)));
    fs::remove_file(&state.settings.state_file).expect("remove state");
    assert!(!allocation_is_open(Some(&reopened)));
}

#[cfg(unix)]
#[test]
fn insecure_file_directory_symlink_and_hardlink_close_admission() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let state = AdmissionState::new();
    let now = AdmissionState::now();
    state.write(now - 1, now + 60);
    fs::set_permissions(
        &state.settings.state_file,
        fs::Permissions::from_mode(0o620),
    )
    .expect("writable state");
    assert!(!allocation_is_open(Some(&state.settings)));
    state.write(now - 1, now + 60);
    let parent = state.settings.state_file.parent().expect("parent");
    fs::set_permissions(parent, fs::Permissions::from_mode(0o770)).expect("writable directory");
    assert!(!allocation_is_open(Some(&state.settings)));
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).expect("private directory");
    let real = parent.join("real.json");
    fs::rename(&state.settings.state_file, &real).expect("move state");
    symlink(&real, &state.settings.state_file).expect("symlink state");
    assert!(!allocation_is_open(Some(&state.settings)));
    fs::remove_file(&state.settings.state_file).expect("remove symlink");
    fs::hard_link(&real, &state.settings.state_file).expect("hardlink state");
    assert!(!allocation_is_open(Some(&state.settings)));
}

#[test]
fn admission_settings_reject_invalid_paths_and_bounds() {
    assert!(
        ConfigPatch::from_toml(
            "version = 1\n\n[admission]\nmax_window_seconds = 1800\nstate_file = \"relative.json\"\n"
        )
        .is_err()
    );
    assert!(ConfigPatch::from_toml("version = 1\n\n[admission]\nmax_window_seconds = 0\nstate_file = \"/run/outrider/admission.json\"\n").is_err());
    assert!(ConfigPatch::from_toml("version = 1\n\n[admission]\nmax_window_seconds = 1800\nstate_file = \"/run/outrider/admission.json\"\n").is_ok());
}
