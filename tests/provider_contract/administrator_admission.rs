use super::*;
#[path = "../fixtures/admission_state.rs"]
mod admission_state;
use admission_state::AdmissionState;

#[tokio::test]
async fn closing_window_during_version_probe_prevents_create_spawn() {
    let directory = TestDirectory::new();
    let binding = fixture_binding(&directory);
    let registry = fixture_registry(
        binding.clone(),
        "admission_version_barrier",
        &directory,
        ProcessLimits::default(),
    );
    let state = AdmissionState::new();
    let now = AdmissionState::now();
    state.write(now - 1, now + 600);
    let marker = directory.path().join("marker");
    let create = request("CreateInstance");
    let requirements = requirements();
    let pending = registry.invoke(
        &binding.id,
        &create,
        Some(&requirements),
        Some(&state.settings),
        deadline(),
    );
    let close_window = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() {
            assert!(
                Instant::now() < deadline,
                "version probe must reach barrier"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        state.write(now - 1800, now);
        fs::write(marker.with_extension("release"), "release").expect("release version probe");
    };
    let (result, ()) = tokio::join!(pending, close_window);
    assert!(matches!(
        result,
        Err(ProviderError::Local(LocalFailure::AdmissionClosed))
    ));
    assert_eq!(
        fs::read_to_string(directory.path().join("launch.log")).expect("launch log"),
        "GetVersion\n"
    );
    assert!(!directory.path().join("record.json").exists());
}
