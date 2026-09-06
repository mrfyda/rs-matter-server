//! Integration tests for the Matter controller initialization.
//!
//! Verifies that we can create a new fabric, persist it to a temp directory,
//! and load it back on a second invocation.

use rs_matter_server::matter::controller::{init_matter, FabricConfig};

/// Test 1: Create a new fabric and verify it's installed.
#[test]
fn test_create_fabric() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().to_str().unwrap().to_string();
    let config = FabricConfig::default();

    let matter = init_matter(&path, &config).expect("init_matter");

    // Verify a fabric is installed.
    assert!(matter.has_fabrics(), "expected a fabric to be installed");

    // Verify the fabric ID matches our config.
    let fabric_id = matter.with_state(|state| state.fabrics.iter().next().map(|f| f.fabric_id()));
    assert_eq!(fabric_id, Some(config.fabric_id));
}

/// Test 2: Reload an existing fabric from disk.
#[test]
fn test_reload_fabric() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let path = tmp.path().to_str().unwrap().to_string();
    let config = FabricConfig::default();

    // First init: creates the fabric.
    {
        let matter = init_matter(&path, &config).expect("init_matter");
        assert!(matter.has_fabrics());
    }

    // Second init: should load from storage.
    let matter2 = init_matter(&path, &config).expect("reload");
    assert!(
        matter2.has_fabrics(),
        "fabric should be loaded from storage"
    );
}

/// Fabric creation must not be flaky.
///
/// CI hit `InvalidData` from `init_matter` once, on x86_64, where this suite
/// otherwise passes. Certificate and CSR encoding both embed DER-encoded
/// ECDSA signatures whose length varies with the random values inside them,
/// so a rare encoding case is the obvious suspect. Run with `--ignored` to
/// hunt for it.
#[test]
#[ignore]
fn fabric_creation_is_not_flaky() {
    // Roughly one creation in three hundred has failed, so a short run proves
    // nothing. Override with FABRIC_FLAKE_ITERATIONS when hunting.
    let iterations: usize = std::env::var("FABRIC_FLAKE_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(300);

    let mut failures = Vec::new();
    for attempt in 0..iterations {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().to_str().unwrap().to_string();
        if let Err(error) = init_matter(&path, &FabricConfig::default()) {
            failures.push(format!("attempt {attempt}: {error:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {iterations} fabric creations failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
