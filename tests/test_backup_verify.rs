//! Backup verification tests.
//!
//! These tests validate that the backup export/restore workflow produces
//! correct and verifiable output.  They run inside the Soroban test
//! environment and do not require external network access.

use asset_registry::{AssetRegistry, AssetRegistryClient};
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{vec, Address, Env, String, Symbol};

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
=======

/// Helper: register a single asset and return its ID, type, and owner.
fn register_test_asset(e: &Env, client: &AssetRegistryClient, owner: &Address, idx: u32) -> u64 {
    let asset_type = Symbol::new(e, "GENSET");
    let metadata = String::from_str(e, &format!("Backup test asset #{} — diesel generator", idx));
    let serial = String::from_str(e, &format!("SN-BACKUP-TEST-{:04}", idx));

    // Ensure the asset type is in the allowlist.
    // (In test we may need the admin to add it first.)
    client.register_asset(&asset_type, &metadata, &serial, owner)
}

/// Test that we can enumerate all assets by iterating through asset IDs
/// up to the asset count and retrieve each one — this simulates the
/// backup export flow.
#[test]
fn test_backup_export_all_assets() {
    let e = Env::default();
    e.mock_all_auths();

    // Deploy asset registry, engineer registry, and lifecycle
    let admin = Address::generate(&e);
    let owner = Address::generate(&e);
    let engineer = Address::generate(&e);

    let ar_id = e.register(AssetRegistry, ());
    let ar_client = AssetRegistryClient::new(&e, &ar_id);
    ar_client.initialize_admin(&admin, &admin);

    // Register asset types and assets
    let asset_type = Symbol::new(&e, "GENSET");
    ar_client.add_asset_type(&admin, &asset_type);

    // Register multiple assets
    let mut asset_ids: Vec<u64> = Vec::new(&e);
    for i in 0..5 {
        let id = register_test_asset(&e, &ar_client, &owner, i + 1);
        asset_ids.push_back(id);
    }

    // Verify asset count matches
    let count = ar_client.get_asset_count();
    assert_eq!(count, 5);
    assert_eq!(asset_ids.len(), 5);

    // Simulate backup: iterate all asset IDs and retrieve each one
    let mut retrieved = 0u32;
    for id in 1..=count {
        let asset = ar_client.get_asset(&id);
        assert_eq!(asset.asset_id, id);
        assert_eq!(asset.asset_type, asset_type);
        assert!(asset.metadata.contains(&String::from_str(&e, "Backup test")));
        assert_eq!(asset.owner, owner);
        retrieved += 1;
    }
    assert_eq!(retrieved, 5);
}

/// Test that asset metadata survives round-trip: register → retrieve → verify.
#[test]
fn test_backup_roundtrip_metadata_integrity() {
    let e = Env::default();
    e.mock_all_auths();

    let admin = Address::generate(&e);
    let owner = Address::generate(&e);

    let ar_id = e.register(AssetRegistry, ());
    let ar_client = AssetRegistryClient::new(&e, &ar_id);
    ar_client.initialize_admin(&admin, &admin);

    let asset_type = Symbol::new(&e, "TURBINE");
    ar_client.add_asset_type(&admin, &asset_type);

    let metadata = String::from_str(&e, "Siemens SGT-800 gas turbine, 50 MW, s/n: T2024-001");
    let serial = String::from_str(&e, "T2024-001");

    let id = ar_client.register_asset(&asset_type, &metadata, &serial, &owner);

    // Retrieve and verify all fields match
    let asset = ar_client.get_asset(&id);
    assert_eq!(asset.asset_id, id);
    assert_eq!(asset.asset_type, asset_type);
    assert_eq!(asset.metadata, metadata);
    assert_eq!(asset.serial_number, serial);
    assert_eq!(asset.owner, owner);
    assert_eq!(asset.metadata_version, 0);

    // Update metadata and verify version increments
    let new_meta = String::from_str(&e, "Siemens SGT-800 after 2024 overhaul");
    ar_client.update_asset_metadata(&id, &owner, &new_meta);

    let updated = ar_client.get_asset(&id);
    assert_eq!(updated.metadata, new_meta);
    assert_eq!(updated.metadata_version, 1);

    // Metadata history should be preserved
    let history = ar_client.get_metadata_history(&id);
    assert_eq!(history.len(), 1); // one version bump
    assert_eq!(history.first().unwrap().version, 1);
}

/// Test that the asset count does not decrease after a deregistration,
/// ensuring backup iterators never skip IDs.
#[test]
fn test_backup_asset_count_monotonic() {
    let e = Env::default();
    e.mock_all_auths();

    let admin = Address::generate(&e);
    let owner = Address::generate(&e);

    let ar_id = e.register(AssetRegistry, ());
    let ar_client = AssetRegistryClient::new(&e, &ar_id);
    ar_client.initialize_admin(&admin, &admin);

    let asset_type = Symbol::new(&e, "PUMP");
    ar_client.add_asset_type(&admin, &asset_type);

    let meta = String::from_str(&e, "Centrifugal pump");
    let serial_a = String::from_str(&e, "PUMP-A-001");
    let serial_b = String::from_str(&e, "PUMP-B-002");

    let id_a = ar_client.register_asset(&asset_type, &meta, &serial_a, &owner);
    let id_b = ar_client.register_asset(&asset_type, &meta, &serial_b, &owner);

    assert_eq!(ar_client.get_asset_count(), 2);

    // Deregister asset A (with timelock)
    ar_client.propose_deregister_asset(&owner, &id_a);
    e.jump(48 * 60 * 60 + 1); // 48 hours + 1 second
    ar_client.execute_deregister_asset(&owner, &id_a);

    // Asset count should NOT decrease (monotonic counter)
    assert_eq!(ar_client.get_asset_count(), 2);

    // Asset A should no longer be retrievable
    assert!(!ar_client.asset_exists(&id_a));

    // Asset B should still be retrievable
    assert!(ar_client.asset_exists(&id_b));
    let asset_b = ar_client.get_asset(&id_b);
    assert_eq!(asset_b.asset_id, id_b);
}

/// Test that backup manifest fields are populated correctly when programmatically
/// constructing a backup manifest from on-chain data.
#[test]
fn test_backup_manifest_generation() {
    let e = Env::default();
    e.mock_all_auths();

    let admin = Address::generate(&e);
    let owner = Address::generate(&e);

    let ar_id = e.register(AssetRegistry, ());
    let ar_client = AssetRegistryClient::new(&e, &ar_id);
    ar_client.initialize_admin(&admin, &admin);

    let asset_type = Symbol::new(&e, "COMPRESSOR");
    ar_client.add_asset_type(&admin, &asset_type);

    // Register 3 assets
    for i in 0..3 {
        let meta = String::from_str(&e, &format!("Air compressor unit {}", i + 1));
        let serial = String::from_str(&e, &format!("CMP-{:04}", i + 1));
        ar_client.register_asset(&asset_type, &meta, &serial, &owner);
    }

    // Collect backup facts
    let total = ar_client.get_asset_count();
    assert_eq!(total, 3);

    let mut serials: Vec<String> = Vec::new(&e);
    let mut metadatas: Vec<String> = Vec::new(&e);
    for id in 1..=total {
        let a = ar_client.get_asset(&id);
        serials.push_back(a.serial_number.clone());
        metadatas.push_back(a.metadata.clone());
    }

    // Verify all expected serial numbers exist
    let expected: Vec<String> = vec![
        &e,
        String::from_str(&e, "CMP-0001"),
        String::from_str(&e, "CMP-0002"),
        String::from_str(&e, "CMP-0003"),
    ];
    for exp in expected.iter() {
        assert!(serials.contains(&exp));
    }
    assert_eq!(metadatas.len(), 3);
}

// ---------------------------------------------------------------------------
// Shell script tests (backup.sh / restore.sh)
//
// These exercise the operational scripts directly.  They require a POSIX
// shell and are skipped gracefully on platforms where one is unavailable.
// ---------------------------------------------------------------------------

/// Locate the repository root (the directory containing `backup.sh`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("contract crate should live inside the repository")
        .to_path_buf()
}

/// Return true when a usable `sh` is available.
fn have_sh() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("exit 0")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create a unique temporary directory for a test.
fn unique_tmp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "stellar-backup-test-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write an executable shell script.
fn write_script(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write script");
    let mut perms = fs::metadata(path).expect("stat script").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod script");
}

/// #1839: backup.sh must fail (non-zero exit) when the asset count read fails
/// instead of silently writing `asset_count: 0` and reporting success.
#[test]
fn test_backup_fails_when_asset_count_read_fails() {
    if !have_sh() {
        eprintln!("skipping: no POSIX shell available");
        return;
    }

    let root = repo_root();
    let backup = root.join("backup.sh");
    if !backup.exists() {
        eprintln!("skipping: backup.sh not found at {:?}", backup);
        return;
    }

    let tmp = unique_tmp_dir("backup-fail");
    let bin = tmp.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");

    // Stub `stellar` so that any contract invocation fails.
    write_script(
        &bin.join("stellar"),
        "#!/bin/sh\necho 'simulated RPC outage' >&2\nexit 1\n",
    );

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new("sh")
        .arg(&backup)
        .current_dir(&tmp)
        .env("PATH", path_env)
        .env("STELLAR_NETWORK", "testnet")
        .env("CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
        .output()
        .expect("run backup.sh");

    assert!(
        !output.status.success(),
        "backup.sh must exit non-zero when the asset count read fails.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // No manifest claiming a valid empty backup should be produced.
    let manifest = tmp.join("backup").join("manifest.json");
    if manifest.exists() {
        let contents = fs::read_to_string(&manifest).unwrap_or_default();
        assert!(
            !contents.contains("asset_count: 0") && !contents.contains("\"asset_count\": 0"),
            "backup.sh must not write asset_count: 0 on failure, got:\n{}",
            contents
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}

/// #1836: in dry-run mode restore.sh must report assets as "would restore"
/// rather than counting them as actually restored.
#[test]
fn test_restore_dry_run_reports_would_restore() {
    if !have_sh() {
        eprintln!("skipping: no POSIX shell available");
        return;
    }

    let root = repo_root();
    let restore = root.join("restore.sh");
    if !restore.exists() {
        eprintln!("skipping: restore.sh not found at {:?}", restore);
        return;
    }

    let tmp = unique_tmp_dir("restore-dry-run");
    let bin = tmp.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");

    // Stub `stellar` so that writes are never actually performed.
    write_script(
        &bin.join("stellar"),
        "#!/bin/sh\necho 'stub: no-op' >&2\nexit 0\n",
    );

    // Fixture backup: a manifest plus two asset files.
    let backup_dir = tmp.join("backup");
    let assets_dir = backup_dir.join("assets");
    fs::create_dir_all(&assets_dir).expect("create assets dir");
    fs::write(
        backup_dir.join("manifest.json"),
        "asset_count: 2\nnetwork: testnet\n",
    )
    .expect("write manifest");
    for id in 1..=2u32 {
        fs::write(
            assets_dir.join(format!("asset_{}.json", id)),
            format!(
                "{{\"asset_id\": {}, \"asset_type\": \"GENSET\", \"metadata\": \"gen {}\", \"serial_number\": \"SN-{}\", \"owner\": \"GOWNER\"}}",
                id, id, id
            ),
        )
        .expect("write asset fixture");
    }

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::new("sh")
        .arg(&restore)
        .arg("--dry-run")
        .arg(&backup_dir)
        .current_dir(&tmp)
        .env("PATH", path_env)
        .env("STELLAR_NETWORK", "testnet")
        .env("CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
        .output()
        .expect("run restore.sh --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    let combined = format!("{}\n{}", stdout, stderr);

    assert!(
        output.status.success(),
        "restore.sh --dry-run should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // The summary must mention a non-zero "would restore" count.
    assert!(
        combined.contains("would restore"),
        "dry-run summary should report a 'would restore' counter, got:\n{}",
        combined
    );
    assert!(
        !combined.contains("would restore: 0")
            && !combined.contains("would restore 0")
            && !combined.contains("would restore:  0"),
        "dry-run summary should report a non-zero 'would restore' count, got:\n{}",
        combined
    );

    // And it must not claim assets were actually restored.
    assert!(
        !combined.contains("restored: 2") && !combined.contains("restored 2"),
        "dry-run must not count assets as actually restored, got:\n{}",
        combined
    );

    let _ = fs::remove_dir_all(&tmp);
}

/// #1838: restore.sh must write an `id_map.json` mapping original IDs to new
/// IDs, and must fail when the map is incomplete.
#[test]
fn test_restore_writes_and_validates_id_map() {
    if !have_sh() {
        eprintln!("skipping: no POSIX shell available");
        return;
    }

    let root = repo_root();
    let restore = root.join("restore.sh");
    if !restore.exists() {
        eprintln!("skipping: restore.sh not found at {:?}", restore);
        return;
    }

    let tmp = unique_tmp_dir("restore-id-map");
    let bin = tmp.join("bin");
    fs::create_dir_all(&bin).expect("create bin dir");

    // Stub `stellar` so that writes are never actually performed.
    write_script(
        &bin.join("stellar"),
        "#!/bin/sh\necho 'stub: no-op' >&2\nexit 0\n",
    );

    let backup_dir = tmp.join("backup");
    let assets_dir = backup_dir.join("assets");
    fs::create_dir_all(&assets_dir).expect("create assets dir");
    fs::write(
        backup_dir.join("manifest.json"),
        "asset_count: 2\nnetwork: testnet\n",
    )
    .expect("write manifest");
    for id in 1..=2u32 {
        fs::write(
            assets_dir.join(format!("asset_{}.json", id)),
            format!(
                "{{\"asset_id\": {}, \"asset_type\": \"GENSET\", \"metadata\": \"gen {}\", \"serial_number\": \"SN-{}\", \"owner\": \"GOWNER\"}}",
                id, id, id
            ),
        )
        .expect("write asset fixture");
    }

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    // A dry-run restore should still produce an id_map.json covering every
    // asset in the backup.
    let output = Command::new("sh")
        .arg(&restore)
        .arg("--dry-run")
        .arg(&backup_dir)
        .current_dir(&tmp)
        .env("PATH", path_env.clone())
        .env("STELLAR_NETWORK", "testnet")
        .env("CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
        .output()
        .expect("run restore.sh --dry-run");

    assert!(
        output.status.success(),
        "restore.sh --dry-run should succeed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let id_map = tmp.join("id_map.json");
    assert!(
        id_map.exists(),
        "restore.sh should write id_map.json (looked in {:?})",
        id_map
    );
    let map_contents = fs::read_to_string(&id_map).expect("read id_map.json");
    assert!(
        map_contents.contains('1') && map_contents.contains('2'),
        "id_map.json should map every original asset ID, got:\n{}",
        map_contents
    );

    // Now simulate an incomplete map: remove one entry and re-run the
    // validation path.  The restore must fail.
    fs::write(&id_map, "{\"1\": 101}\n").expect("write incomplete id_map.json");

    let output = Command::new("sh")
        .arg(&restore)
        .arg("--validate-id-map")
        .arg(&backup_dir)
        .current_dir(&tmp)
        .env("PATH", path_env)
        .env("STELLAR_NETWORK", "testnet")
        .env("CONTRACT_ID", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM")
        .output()
        .expect("run restore.sh --validate-id-map");

    assert!(
        !output.status.success(),
        "restore.sh must fail when id_map.json is incomplete.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let _ = fs::remove_dir_all(&tmp);
}
