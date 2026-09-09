//! Adopting a matterjs-server installation, end to end.
//!
//! The fixtures here are built from certificates rs-matter generates, not from
//! recorded bytes, so what is being tested is the whole path a migrating user
//! takes: a matter.js storage directory in, a working fabric plus node list
//! and settings out, and the same fabric still there on the next start.
//!
//! What cannot be tested here is the only thing that finally matters — that a
//! device already commissioned by matterjs-server accepts the imported
//! identity — because that needs the device. What is checked instead is that
//! every value which decides that outcome (root certificate, the controller's
//! NOC and operational key, the IPK, the node and fabric ids) crosses over
//! unchanged.

use std::io::Write;
use std::path::Path;

use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN;
use rs_matter::crypto::{default_crypto, CanonPkcSecretKey, Crypto, SecretKey, SigningSecretKey};
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;
use rs_matter_server::matter::controller::{
    init_controller, init_controller_with_import, FabricConfig, FabricOrigin,
};
use rs_matter_server::storage::NodeStore;
use serde_json::{json, Value};

const CSR_BUF_LEN: usize = 512;
const FABRIC_ID: u64 = 0x1122_3344_5566_7788;
const CONTROLLER_NODE_ID: u64 = 112233;
const VENDOR_ID: u16 = 0xFFF1;

/// A fabric's certificates and keys, as matter.js would hold them.
struct Material {
    root_cert: Vec<u8>,
    root_key: Vec<u8>,
    /// Present only for the three-tier variant.
    icac: Vec<u8>,
    icac_key: Vec<u8>,
    noc: Vec<u8>,
    operational_key: Vec<u8>,
    ipk: Vec<u8>,
}

/// Build a real fabric's worth of certificates.
///
/// Retried for the same reason the server retries: rs-matter's generators
/// reject a small fraction of random keys, and a test that fails once in two
/// hundred runs is worse than no test.
fn generate(with_icac: bool) -> Material {
    let mut last = String::new();
    for _ in 0..5 {
        match generate_once(with_icac) {
            Ok(material) => return material,
            Err(error) => last = error,
        }
    }
    panic!("could not generate fabric material: {}", last);
}

fn generate_once(with_icac: bool) -> Result<Material, String> {
    let crypto = default_crypto(rand_core::OsRng, rs_matter::dm::devices::test::DAC_PRIVKEY);

    let mut rcac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut rcac_gen = RcacGenerator::new(&mut rcac_buf);
    let (rcac_priv, rcac) = rcac_gen
        .generate(&crypto, FABRIC_ID, VALID_FOREVER)
        .map_err(|e| format!("rcac: {:?}", e.code()))?;
    let root_key = rcac_priv.access().to_vec();
    let root_cert = rcac.to_vec();

    let mut icac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut icac_gen = IcacGenerator::new(&mut icac_buf);
    let (icac, icac_key, signing_key) = if with_icac {
        let (icac_priv, icac) = icac_gen
            .generate(&crypto, rcac_priv.reference(), rcac, VALID_FOREVER)
            .map_err(|e| format!("icac: {:?}", e.code()))?;
        let key = icac_priv.access().to_vec();
        (icac.to_vec(), key.clone(), key)
    } else {
        (Vec::new(), Vec::new(), root_key.clone())
    };

    // The controller's own operational key and certificate.
    let controller_key = crypto
        .generate_secret_key()
        .map_err(|e| format!("operational key: {:?}", e.code()))?;
    let mut csr_buf = [0u8; CSR_BUF_LEN];
    let csr = controller_key
        .csr(&mut csr_buf)
        .map_err(|e| format!("csr: {:?}", e.code()))?;
    let mut operational_key = CanonPkcSecretKey::new();
    controller_key
        .write_canon(&mut operational_key)
        .map_err(|e| format!("encoding the operational key: {:?}", e.code()))?;

    let signing = CanonPkcSecretKey::try_from(signing_key.as_slice())
        .map_err(|e| format!("signing key: {:?}", e))?;
    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_gen = NocGenerator::create(signing.reference(), &root_cert, &icac, &mut noc_buf)
        .map_err(|e| format!("noc generator: {:?}", e.code()))?;
    let noc = noc_gen
        .generate(&crypto, csr, CONTROLLER_NODE_ID, &[], VALID_FOREVER)
        .map_err(|e| format!("noc: {:?}", e.code()))?
        .to_vec();

    Ok(Material {
        root_cert,
        root_key,
        icac,
        icac_key,
        noc,
        operational_key: operational_key.access().to_vec(),
        ipk: (0u8..16).collect(),
    })
}

// -- writing a matter.js storage directory -------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{:02x}", byte)).collect()
}

/// A byte string in matter.js's tagged encoding.
fn tagged_bytes(bytes: &[u8]) -> Value {
    Value::String(format!(
        r#"{{"__object__":"Uint8Array","__value__":"{}"}}"#,
        hex(bytes)
    ))
}

fn tagged_bigint(value: u64) -> Value {
    Value::String(format!(
        r#"{{"__object__":"BigInt","__value__":"{}"}}"#,
        value
    ))
}

/// As stored, optionally carrying matter.js's own compressed fabric id.
fn fabric_config_claiming(material: &Material, compressed_fabric_id: Option<u64>) -> Value {
    let mut fabric = json!({
        "fabricIndex": 1,
        "fabricId": tagged_bigint(FABRIC_ID),
        "nodeId": tagged_bigint(CONTROLLER_NODE_ID),
        "rootNodeId": tagged_bigint(CONTROLLER_NODE_ID),
        "rootVendorId": VENDOR_ID,
        "rootCert": tagged_bytes(&material.root_cert),
        "operationalCert": tagged_bytes(&material.noc),
        "identityProtectionKey": tagged_bytes(&material.ipk),
        "keyPair": {
            "privateKey": tagged_bytes(&material.operational_key),
            "publicKey": tagged_bytes(&[4u8; 65]),
        },
        "label": "Living Room",
    });
    if !material.icac.is_empty() {
        fabric["intermediateCACert"] = tagged_bytes(&material.icac);
    } else {
        // matter.js writes the field as an explicit `undefined`, which is not
        // the same thing as leaving it out.
        fabric["intermediateCACert"] = Value::String(r#"{"__object__":"Undefined"}"#.into());
    }
    if let Some(id) = compressed_fabric_id {
        fabric["operationalId"] = tagged_bytes(&id.to_be_bytes());
    }
    fabric
}

fn credentials_claiming(material: &Material, compressed_fabric_id: Option<u64>) -> Value {
    let mut credentials = json!({
        "fabric": fabric_config_claiming(material, compressed_fabric_id),
        "rootCertId": 0,
        "rootCertBytes": tagged_bytes(&material.root_cert),
        "rootKeyIdentifier": tagged_bytes(&[7u8; 20]),
        "nextCertificateId": 5,
        "rootKeyPair": {
            "privateKey": tagged_bytes(&material.root_key),
            "publicKey": tagged_bytes(&[4u8; 65]),
        },
    });
    if !material.icac.is_empty() {
        credentials["icacCertId"] = json!(1);
        credentials["icacCertBytes"] = tagged_bytes(&material.icac);
        credentials["icacKeyIdentifier"] = tagged_bytes(&[8u8; 20]);
        credentials["icacKeyPair"] = json!({
            "privateKey": tagged_bytes(&material.icac_key),
            "publicKey": tagged_bytes(&[4u8; 65]),
        });
    }
    credentials
}

/// Write a namespace the way the `wal` driver does — a gzipped snapshot plus a
/// log of later commits — which is what matterjs-server writes today.
fn write_wal_namespace(path: &Path, snapshot: Value, commits: &[Value]) {
    std::fs::create_dir_all(path.join("wal")).unwrap();
    std::fs::write(path.join("driver.json"), r#"{"kind":"wal","type":"kv"}"#).unwrap();

    let document = json!({
        "commitId": { "segment": 0, "offset": 0 },
        "ts": 1_700_000_000_000u64,
        "data": snapshot,
    });
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(serde_json::to_string(&document).unwrap().as_bytes())
        .unwrap();
    std::fs::write(path.join("snapshot.json.gz"), encoder.finish().unwrap()).unwrap();

    let lines: Vec<String> = commits
        .iter()
        .map(|commit| serde_json::to_string(commit).unwrap())
        .collect();
    std::fs::write(path.join("wal/00000000.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// Write a namespace the way the older `file` driver does: one file per key.
fn write_file_namespace(path: &Path, values: &[(&str, &str, Value)]) {
    std::fs::create_dir_all(path).unwrap();
    for (context, key, value) in values {
        std::fs::write(
            path.join(format!("{}.{}", context, key)),
            serde_json::to_string(value).unwrap(),
        )
        .unwrap();
    }
}

/// A complete source directory: the controller in `wal`, the settings in
/// `file`, which is a mix a real upgrade path produces.
fn write_source(root: &Path, material: &Material) {
    write_source_claiming(root, material, None)
}

fn write_source_claiming(root: &Path, material: &Material, compressed_fabric_id: Option<u64>) {
    let snapshot = json!({
        "credentials": credentials_claiming(material, compressed_fabric_id),
        // Only the first node is in the snapshot; the second arrives in the
        // log, as it would if it were commissioned after the last snapshot.
        "nodes": { "commissionedNodes": [[tagged_bigint(1), {}]] },
        "nodes.peer1.endpoints.0.commissioning": {
            "peerAddress": { "fabricIndex": 1, "nodeId": tagged_bigint(1) },
            "commissionedAt": 1_700_000_000_000u64,
            "fabricIndexOnPeer": 3,
            "addresses": [
                { "type": "udp", "ip": "fd11::1", "port": 5540 },
                { "type": "udp", "ip": "fd11::2", "port": 5540 }
            ],
        },
    });

    let commits = vec![
        // Offset 0 is covered by the snapshot: a reader that replays it anyway
        // would resurrect a node the user removed.
        json!({ "ts": 1, "ops": [
            { "op": "upd", "key": "nodes", "values": { "commissionedNodes": [[tagged_bigint(99), {}]] } }
        ] }),
        json!({ "ts": 2, "ops": [
            { "op": "upd", "key": "nodes", "values": {
                "commissionedNodes": [[tagged_bigint(1), {}], [tagged_bigint(2), {}]]
            } },
            { "op": "upd", "key": "nodes.peer2.endpoints.0.commissioning", "values": {
                "peerAddress": { "fabricIndex": 1, "nodeId": tagged_bigint(2) },
                "commissionedAt": 1_700_000_100_000u64
            } }
        ] }),
    ];

    write_wal_namespace(&root.join("server"), snapshot, &commits);

    write_file_namespace(
        &root.join("config"),
        &[
            ("values", "fabricLabel", json!("Living Room")),
            ("values", "nextNodeId", json!(7)),
            ("values", "wifiSsid", json!("home-network")),
            ("values", "wifiCredentials", json!("hunter2")),
            ("values", "threadDataset", json!("0e080000000000010000")),
            (
                "values",
                "additionalWifiCredentials",
                json!([{ "id": "guest", "ssid": "guest-net", "credentials": "letmein" }]),
            ),
        ],
    );
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

// -- the tests -----------------------------------------------------------

#[test]
fn a_matterjs_installation_is_adopted_whole() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let material = generate(false);
    write_source(source.path(), &material);

    let import = rs_matter_server::migrate::read(source.path(), None).expect("read the source");
    let controller = init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .expect("install the imported fabric");
    assert_eq!(controller.origin, FabricOrigin::Imported);
    rs_matter_server::migrate::apply_state(&import, target.path()).expect("write the state");

    // The identity a device checks: same fabric, same controller node, and the
    // root certificate byte for byte.
    let (fabric_id, node_id, root_ca, icac, label) = controller.matter.with_state(|state| {
        let fabric = state.fabrics.iter().next().expect("a fabric");
        (
            fabric.fabric_id(),
            fabric.node_id(),
            fabric.root_ca().to_vec(),
            fabric.icac().to_vec(),
            fabric.label().to_string(),
        )
    });
    assert_eq!(fabric_id, FABRIC_ID);
    assert_eq!(node_id, CONTROLLER_NODE_ID);
    assert_eq!(root_ca, material.root_cert);
    assert!(icac.is_empty(), "matter.js signs NOCs with the root");
    assert_eq!(label, "Living Room");

    // The key that will sign certificates for devices commissioned from now on.
    let issuer = std::fs::read(target.path().join("controller-icac-key.bin")).unwrap();
    assert_eq!(issuer, material.root_key);

    // The node list: both nodes, one from the snapshot and one from the log.
    let nodes = NodeStore::load(target.path().join("nodes.json")).unwrap();
    assert_eq!(nodes.len(), 2);
    let first = nodes.get_stored(1).expect("node 1");
    assert_eq!(first.data.date_commissioned, "2023-11-14T22:13:20.000Z");
    assert_eq!(first.ip_addresses, vec!["fd11::1", "fd11::2"]);
    assert_eq!(first.device_fabric_index, Some(3));
    assert!(
        !first.data.available && first.data.attributes.is_empty(),
        "nothing has been read from the device yet"
    );
    assert!(nodes.contains(2), "a node added after the snapshot");
    assert!(
        !nodes.contains(99),
        "a commit the snapshot already covers must not be replayed"
    );

    // The settings, including the named credential list.
    let config = read_json(&target.path().join("config.json"));
    assert_eq!(config["fabric_label"], "Living Room");
    assert_eq!(config["wifi_ssid"], "home-network");
    assert_eq!(config["wifi_credentials"], "hunter2");
    assert_eq!(config["thread_dataset"], "0e080000000000010000");
    assert_eq!(config["additional_wifi"][0]["id"], "guest");
    assert_eq!(config["additional_wifi"][0]["credentials"], "letmein");
    assert_eq!(
        config["next_node_id"], 7,
        "the source's counter is carried over so ids are never reused"
    );
}

#[test]
fn a_three_tier_fabric_keeps_issuing_through_its_intermediate() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let material = generate(true);
    write_source(source.path(), &material);

    let import = rs_matter_server::migrate::read(source.path(), None).unwrap();
    let controller = init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .unwrap();

    let icac = controller
        .matter
        .with_state(|state| state.fabrics.iter().next().unwrap().icac().to_vec());
    assert_eq!(
        icac, material.icac,
        "the intermediate certificate travels with the fabric, because CASE sends it"
    );
    assert_eq!(
        std::fs::read(target.path().join("controller-icac-key.bin")).unwrap(),
        material.icac_key,
        "NOCs must go on being signed by the certificate that signed the last ones"
    );
}

#[test]
fn the_imported_fabric_survives_a_restart_and_the_flag_stays_harmless() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let material = generate(false);
    write_source(source.path(), &material);

    let import = rs_matter_server::migrate::read(source.path(), None).unwrap();
    init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .unwrap();

    // A restart with the flag still in place, as it would be in a compose
    // file: the fabric is loaded, not imported again.
    let restarted = init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .unwrap();
    assert_eq!(restarted.origin, FabricOrigin::Loaded);
    let (fabric_id, node_id) = restarted.matter.with_state(|state| {
        let fabric = state.fabrics.iter().next().unwrap();
        (fabric.fabric_id(), fabric.node_id())
    });
    assert_eq!(fabric_id, FABRIC_ID);
    assert_eq!(node_id, CONTROLLER_NODE_ID);

    // And a plain start, without the flag, finds the same fabric.
    let plain = init_controller(target.path().to_str().unwrap(), &FabricConfig::default()).unwrap();
    assert_eq!(plain.origin, FabricOrigin::Loaded);
}

#[test]
fn an_import_never_lands_on_a_server_that_already_has_a_fabric() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let material = generate(false);
    write_source(source.path(), &material);

    // A server that has been running on its own fabric.
    let own = init_controller(target.path().to_str().unwrap(), &FabricConfig::default()).unwrap();
    let own_fabric_id = own
        .matter
        .with_state(|state| state.fabrics.iter().next().unwrap().fabric_id());
    drop(own);

    let import = rs_matter_server::migrate::read(source.path(), None).unwrap();
    let controller = init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .unwrap();

    assert_eq!(controller.origin, FabricOrigin::Loaded);
    assert_eq!(
        controller
            .matter
            .with_state(|state| state.fabrics.iter().next().unwrap().fabric_id()),
        own_fabric_id,
        "the server's own fabric must not be replaced"
    );
}

#[test]
fn a_source_that_cannot_be_read_fails_before_anything_is_written() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();

    // Storage that exists but holds no fabric — a path pointed at the wrong
    // directory is the likely mistake.
    std::fs::create_dir_all(source.path().join("config")).unwrap();
    std::fs::write(
        source.path().join("config/driver.json"),
        r#"{"kind":"json"}"#,
    )
    .unwrap();
    std::fs::write(
        source.path().join("config/storage.json"),
        r#"{"values":{"fabricLabel":"Home"}}"#,
    )
    .unwrap();

    let error = rs_matter_server::migrate::read(source.path(), None).unwrap_err();
    assert!(
        error.to_string().contains("no controller fabric"),
        "{}",
        error
    );
    assert_eq!(
        std::fs::read_dir(target.path()).unwrap().count(),
        0,
        "the target must be untouched, so a corrected path can still import"
    );
}

#[test]
fn the_source_directory_is_never_modified() {
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let material = generate(false);
    write_source(source.path(), &material);

    let before = fingerprint(source.path());
    let import = rs_matter_server::migrate::read(source.path(), None).unwrap();
    init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .unwrap();
    rs_matter_server::migrate::apply_state(&import, target.path()).unwrap();

    assert_eq!(
        before,
        fingerprint(source.path()),
        "going back to matterjs-server has to remain possible"
    );
}

#[test]
fn the_compressed_fabric_id_is_checked_against_what_the_source_announced() {
    let material = generate(false);

    // What rs-matter derives from these certificates. matter.js derives it the
    // same way, from the root public key and the fabric id.
    let derived = {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        write_source(source.path(), &material);
        let import = rs_matter_server::migrate::read(source.path(), None).unwrap();
        let controller = init_controller_with_import(
            target.path().to_str().unwrap(),
            &FabricConfig::default(),
            Some(&import.fabric),
        )
        .unwrap();
        controller
            .matter
            .with_state(|state| state.fabrics.iter().next().unwrap().compressed_fabric_id())
    };

    // A source that announces that id imports.
    let agreeing = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    write_source_claiming(agreeing.path(), &material, Some(derived));
    let import = rs_matter_server::migrate::read(agreeing.path(), None).unwrap();
    assert_eq!(import.fabric.compressed_fabric_id, Some(derived));
    assert!(init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    )
    .is_ok());

    // One that announces a different id does not: the certificates and the
    // identity the devices know would not be the same fabric.
    let disagreeing = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    write_source_claiming(disagreeing.path(), &material, Some(derived ^ 1));
    let import = rs_matter_server::migrate::read(disagreeing.path(), None).unwrap();
    let error = match init_controller_with_import(
        target.path().to_str().unwrap(),
        &FabricConfig::default(),
        Some(&import.fabric),
    ) {
        Ok(_) => panic!("a fabric that is not the source's fabric must not be installed"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("do not match"), "{}", error);
}

/// Every file under a directory, with its contents.
fn fingerprint(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                entries.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read(&path).unwrap(),
                ));
            }
        }
    }
    entries.sort();
    entries
}
