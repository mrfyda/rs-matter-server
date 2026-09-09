//! Turning matter.js storage into the values this server needs.
//!
//! Everything here is extraction and validation only — nothing is written.
//! Where matter.js has moved a value between versions, every place it has
//! lived is tried, because the whole point of the import is to work against
//! whatever a user's install happens to hold.

use anyhow::{anyhow, bail, Context, Result};

use super::store::{MatterJsStorage, Namespace};
use super::value::MjValue;

/// Where matterjs-server keeps the server's own settings.
pub const CONFIG_NAMESPACE: &str = "config";
/// The default controller namespace. A multi-fabric install uses
/// `server-<fabricId>-<vendorId>` instead.
pub const DEFAULT_CONTROLLER_NAMESPACE: &str = "server";

/// Contexts the controller fabric has lived in, newest first.
const FABRIC_CONTEXTS: &[&str] = &["credentials", "MatterController", "certificates"];
/// Contexts the certificate authority has lived in, newest first.
const CA_CONTEXTS: &[&str] = &["credentials", "certificates", "RootCertificateManager"];

/// A P-256 private key, as rs-matter wants it.
const SECRET_KEY_LEN: usize = 32;
/// The IPK epoch key.
const EPOCH_KEY_LEN: usize = 16;

/// The fabric, ready to be installed into rs-matter.
///
/// [`Debug`] is written by hand rather than derived: three of these fields are
/// private keys, and a struct that carries them should not be printable into a
/// log by accident.
#[derive(Clone)]
pub struct ImportedFabric {
    pub root_cert: Vec<u8>,
    pub noc: Vec<u8>,
    /// Empty when the fabric's NOCs are signed by the root directly, which is
    /// how matter.js issues them unless it was itself migrated from the Python
    /// server. rs-matter supports both.
    pub icac: Vec<u8>,
    /// The controller's operational key. Reusing it — rather than issuing a
    /// fresh NOC — is what makes already-commissioned devices accept us
    /// without being touched.
    pub operational_key: Vec<u8>,
    pub ipk_epoch_key: Vec<u8>,
    pub vendor_id: u16,
    pub node_id: u64,
    pub fabric_id: u64,
    pub fabric_index: u8,
    pub label: String,
    /// The key that signs NOCs for newly commissioned devices: the ICAC key
    /// when there is one, otherwise the root key.
    pub issuer_key: Vec<u8>,
    pub issuer_is_icac: bool,
    /// The compressed fabric id matter.js computed for this fabric, when it
    /// stored one. It is derived from the root public key and the fabric id,
    /// so rs-matter arrives at it independently — which makes it a free check
    /// that the certificates were read correctly. Devices find this controller
    /// by that value in mDNS, so a mismatch would be invisible until nothing
    /// could be reached.
    pub compressed_fabric_id: Option<u64>,
}

/// One commissioned node.
///
/// Attributes are deliberately absent. matter.js stores them decoded into its
/// own object model, and translating that back into the tag-based JSON the
/// protocol uses would mean re-deriving every struct field tag — a large
/// surface with silent failure modes. The device is the authority anyway, so
/// the first poll after startup fills them in.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportedNode {
    pub node_id: u64,
    /// Milliseconds since the epoch, as matter.js records it.
    pub commissioned_at: Option<u64>,
    /// The fabric slot the *device* gave this controller.
    pub fabric_index_on_peer: Option<u8>,
    pub addresses: Vec<String>,
}

impl std::fmt::Debug for ImportedFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedFabric")
            .field("fabric_id", &format_args!("0x{:016x}", self.fabric_id))
            .field("node_id", &format_args!("0x{:016x}", self.node_id))
            .field("fabric_index", &self.fabric_index)
            .field("vendor_id", &format_args!("0x{:04x}", self.vendor_id))
            .field("label", &self.label)
            .field("root_cert", &format_args!("{} bytes", self.root_cert.len()))
            .field("noc", &format_args!("{} bytes", self.noc.len()))
            .field("icac", &format_args!("{} bytes", self.icac.len()))
            .field("issuer_is_icac", &self.issuer_is_icac)
            .field("compressed_fabric_id", &self.compressed_fabric_id)
            .field("keys", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for ImportedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedConfig")
            .field("fabric_label", &self.fabric_label)
            .field("next_node_id", &self.next_node_id)
            .field(
                "wifi",
                &self
                    .wifi
                    .iter()
                    .map(|(id, ssid, _)| (id, ssid))
                    .collect::<Vec<_>>(),
            )
            .field(
                "thread",
                &self.thread.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The server settings, which map onto our `config.json` one for one.
///
/// [`Debug`] is written by hand: this holds the Wi-Fi passwords.
#[derive(Clone, Default)]
pub struct ImportedConfig {
    pub fabric_label: Option<String>,
    pub next_node_id: Option<u64>,
    /// `(id, ssid, credentials)`, the reserved `default` entry included.
    pub wifi: Vec<(String, String, String)>,
    /// `(id, dataset)`.
    pub thread: Vec<(String, String)>,
}

/// Find the namespace holding the controller fabric.
///
/// `server` is the default, but matterjs-server switches to
/// `server-<fabricId>-<vendorId>` for multi-fabric setups, so a namespace
/// carrying a fabric is accepted under any name when there is exactly one.
pub fn find_controller_namespace<'a>(
    storage: &'a MatterJsStorage,
    requested: Option<&str>,
) -> Result<&'a Namespace> {
    if let Some(name) = requested {
        let namespace = storage.namespace(name)?;
        if fabric_config(namespace).is_none() {
            bail!(
                "the '{}' namespace holds no controller fabric. Namespaces present: {}",
                name,
                storage.namespace_names().join(", ")
            );
        }
        return Ok(namespace);
    }

    if storage.has_namespace(DEFAULT_CONTROLLER_NAMESPACE) {
        let namespace = storage.namespace(DEFAULT_CONTROLLER_NAMESPACE)?;
        if fabric_config(namespace).is_some() {
            return Ok(namespace);
        }
    }

    let candidates: Vec<&Namespace> = storage
        .namespace_names()
        .iter()
        .filter_map(|name| storage.namespace(name).ok())
        .filter(|namespace| fabric_config(namespace).is_some())
        .collect();

    match candidates.len() {
        1 => Ok(candidates[0]),
        0 => bail!(
            "no controller fabric found in any namespace ({}). Nothing was commissioned with \
             this storage directory, or it belongs to a different application",
            storage.namespace_names().join(", ")
        ),
        _ => bail!(
            "several namespaces hold a fabric ({}). This importer migrates one fabric; \
             name the one to import with --import-matterjs-namespace",
            candidates
                .iter()
                .map(|namespace| namespace.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The stored `Fabric.Config`, wherever this matter.js version put it.
fn fabric_config(namespace: &Namespace) -> Option<&MjValue> {
    for context in FABRIC_CONTEXTS {
        if let Some(fabric) = namespace.get(&[context], "fabric") {
            if fabric.get("operationalCert").is_some() {
                return Some(fabric);
            }
        }
    }

    // The node API keeps every fabric in one list instead.
    namespace
        .get(&["fabrics"], "fabrics")?
        .as_array()?
        .iter()
        .find(|fabric| fabric.get("operationalCert").is_some())
}

/// Read the fabric and the key that will sign future device NOCs.
pub fn read_fabric(namespace: &Namespace) -> Result<ImportedFabric> {
    let fabric = fabric_config(namespace)
        .ok_or_else(|| anyhow!("no fabric in the '{}' namespace", namespace.name))?;

    let root_cert = required_bytes(fabric, "rootCert")?;
    let noc = required_bytes(fabric, "operationalCert")?;
    let icac = fabric
        .get("intermediateCACert")
        .and_then(MjValue::as_bytes)
        .unwrap_or_default()
        .to_vec();

    let operational_key = private_key(
        fabric
            .get("keyPair")
            .ok_or_else(|| anyhow!("the stored fabric has no operational key pair"))?,
    )
    .context("reading the controller's operational key")?;

    let ipk_epoch_key = required_bytes(fabric, "identityProtectionKey")?;
    if ipk_epoch_key.len() != EPOCH_KEY_LEN {
        bail!(
            "the identity protection key is {} bytes, expected {}",
            ipk_epoch_key.len(),
            EPOCH_KEY_LEN
        );
    }

    let node_id = required_u64(fabric, "nodeId")?;
    let fabric_id = required_u64(fabric, "fabricId")?;
    let vendor_id = u16::try_from(required_u64(fabric, "rootVendorId")?)
        .context("the stored vendor id does not fit in 16 bits")?;
    let fabric_index = u8::try_from(
        fabric
            .get("fabricIndex")
            .and_then(MjValue::as_u64)
            .unwrap_or(1),
    )
    .context("the stored fabric index does not fit in 8 bits")?;
    let label = fabric
        .get("label")
        .and_then(MjValue::as_str)
        .unwrap_or_default()
        .to_string();

    let (issuer_key, issuer_is_icac) = read_issuer_key(namespace, !icac.is_empty())?;

    Ok(ImportedFabric {
        compressed_fabric_id: compressed_fabric_id(fabric),
        root_cert,
        noc,
        icac,
        operational_key,
        ipk_epoch_key,
        vendor_id,
        node_id,
        fabric_id,
        fabric_index,
        label,
        issuer_key,
        issuer_is_icac,
    })
}

/// matter.js's own compressed fabric id, under either name it has had.
///
/// `globalId` is the current spelling and holds the number; `operationalId` is
/// the older one and holds the same value as eight big-endian bytes.
fn compressed_fabric_id(fabric: &MjValue) -> Option<u64> {
    if let Some(global) = fabric.get("globalId").and_then(MjValue::as_u64) {
        return Some(global);
    }

    let bytes = fabric.get("operationalId").and_then(MjValue::as_bytes)?;
    let bytes: [u8; 8] = bytes.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// The signing key for NOCs issued to devices commissioned from now on.
///
/// A fabric whose own certificate chain runs through an ICAC must keep issuing
/// through that ICAC — the issuer named on a NOC has to match the certificate
/// that signed it — so when the ICAC's key is missing there is no honest way
/// to continue, and the import stops rather than leaving commissioning to fail
/// later against a real device.
fn read_issuer_key(namespace: &Namespace, fabric_has_icac: bool) -> Result<(Vec<u8>, bool)> {
    let context = CA_CONTEXTS
        .iter()
        .find(|context| namespace.get(&[context], "rootCertBytes").is_some())
        .copied();

    let Some(context) = context else {
        bail!(
            "the '{}' namespace holds a fabric but no certificate authority, so this server \
             could not issue certificates to newly commissioned devices",
            namespace.name
        );
    };

    if fabric_has_icac {
        return match namespace.get(&[context], "icacKeyPair") {
            Some(key_pair) => Ok((
                private_key(key_pair).context("reading the intermediate CA key")?,
                true,
            )),
            None => bail!(
                "the fabric's certificates run through an intermediate CA, but its private key \
                 is not in the '{}' storage. matterjs-server could not commission new devices \
                 with this storage either; the fabric has to be recreated",
                namespace.name
            ),
        };
    }

    match namespace.get(&[context], "rootKeyPair") {
        Some(key_pair) => Ok((
            private_key(key_pair).context("reading the root CA key")?,
            false,
        )),
        None => bail!(
            "the certificate authority in '{}' has no root key, so this server could not issue \
             certificates to newly commissioned devices",
            namespace.name
        ),
    }
}

/// A key pair as matter.js stores it: `{privateKey, publicKey}`, or — from
/// older versions — the private key's bytes on their own.
fn private_key(value: &MjValue) -> Result<Vec<u8>> {
    let bytes = match value {
        MjValue::Bytes(bytes) => bytes.clone(),
        _ => value
            .get("privateKey")
            .and_then(MjValue::as_bytes)
            .ok_or_else(|| anyhow!("no private key in the stored key pair"))?
            .to_vec(),
    };

    if bytes.len() != SECRET_KEY_LEN {
        bail!(
            "a stored private key is {} bytes, expected {}",
            bytes.len(),
            SECRET_KEY_LEN
        );
    }
    Ok(bytes)
}

/// A required field, rejecting a value that is absent or not the type it has
/// to be — decoding leaves anything it did not recognise as text, so this is
/// where an unreadable value is caught, with the field named.
fn required_bytes(value: &MjValue, field: &str) -> Result<Vec<u8>> {
    value
        .get(field)
        .and_then(MjValue::as_bytes)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| anyhow!("the stored fabric has no readable '{}'", field))
}

fn required_u64(value: &MjValue, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(MjValue::as_u64)
        .ok_or_else(|| anyhow!("the stored fabric has no readable '{}'", field))
}

/// Every commissioned node, from whichever layout this install uses.
///
/// The list of ids and the per-node detail are stored separately, and either
/// can be present without the other: a node that appears only in
/// `commissionedNodes` is still imported, with no address or date.
pub fn read_nodes(namespace: &Namespace) -> Vec<ImportedNode> {
    let mut nodes: std::collections::BTreeMap<u64, ImportedNode> = Default::default();

    // The controller's own list: `[[nodeId, {}], …]`.
    if let Some(list) = namespace
        .get(&["nodes"], "commissionedNodes")
        .and_then(MjValue::as_array)
    {
        for entry in list {
            let node_id = match entry {
                MjValue::Array(pair) => pair.first().and_then(MjValue::as_u64),
                other => other.as_u64(),
            };
            if let Some(node_id) = node_id {
                record(&mut nodes, node_id);
            }
        }
    }

    // Per-peer state: the addresses, the commissioning date, and the fabric
    // index the device assigned us.
    for peer in namespace.child_contexts(&["nodes"]) {
        let path = ["nodes", peer.as_str(), "endpoints", "0", "commissioning"];
        let Some(commissioning) = namespace.context(&path) else {
            continue;
        };
        let Some(node_id) = commissioning
            .get("peerAddress")
            .and_then(|address| address.get("nodeId"))
            .and_then(MjValue::as_u64)
        else {
            continue;
        };

        let entry = record(&mut nodes, node_id);
        entry.commissioned_at = commissioning
            .get("commissionedAt")
            .and_then(MjValue::as_u64);
        entry.fabric_index_on_peer = commissioning
            .get("fabricIndexOnPeer")
            .and_then(MjValue::as_u64)
            .and_then(|index| u8::try_from(index).ok());
        entry.addresses = commissioning
            .get("addresses")
            .and_then(MjValue::as_array)
            .map(|addresses| {
                addresses
                    .iter()
                    .filter_map(|address| address.get("ip").and_then(MjValue::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
    }

    // The older layout named a context per node instead of per peer.
    for context in namespace.child_contexts(&[]) {
        if let Some(digits) = context.strip_prefix("node-") {
            if let Ok(node_id) = digits.parse::<u64>() {
                record(&mut nodes, node_id);
            }
        }
    }

    nodes.into_values().collect()
}

/// The entry for a node id, created bare if this is the first mention of it.
fn record(
    nodes: &mut std::collections::BTreeMap<u64, ImportedNode>,
    node_id: u64,
) -> &mut ImportedNode {
    nodes.entry(node_id).or_insert(ImportedNode {
        node_id,
        commissioned_at: None,
        fabric_index_on_peer: None,
        addresses: Vec::new(),
    })
}

/// The server settings from the `config` namespace.
pub fn read_config(storage: &MatterJsStorage) -> Result<ImportedConfig> {
    // A storage directory with no config namespace is not an error: the
    // fabric is what matters, and settings simply fall back to defaults.
    if !storage.has_namespace(CONFIG_NAMESPACE) {
        return Ok(ImportedConfig::default());
    }
    let namespace = storage.namespace(CONFIG_NAMESPACE)?;
    let Some(values) = namespace.context(&["values"]) else {
        return Ok(ImportedConfig::default());
    };

    let text = |key: &str| {
        values
            .get(key)
            .and_then(MjValue::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    let mut wifi = Vec::new();
    if let (Some(ssid), Some(credentials)) = (text("wifiSsid"), text("wifiCredentials")) {
        wifi.push(("default".to_string(), ssid, credentials));
    }
    if let Some(entries) = values
        .get("additionalWifiCredentials")
        .and_then(MjValue::as_array)
    {
        for entry in entries {
            let (Some(id), Some(ssid), Some(credentials)) = (
                entry.get("id").and_then(MjValue::as_str),
                entry.get("ssid").and_then(MjValue::as_str),
                entry.get("credentials").and_then(MjValue::as_str),
            ) else {
                continue;
            };
            wifi.push((id.to_string(), ssid.to_string(), credentials.to_string()));
        }
    }

    let mut thread = Vec::new();
    if let Some(dataset) = text("threadDataset") {
        thread.push(("default".to_string(), dataset));
    }
    if let Some(entries) = values
        .get("additionalThreadCredentials")
        .and_then(MjValue::as_array)
    {
        for entry in entries {
            let (Some(id), Some(dataset)) = (
                entry.get("id").and_then(MjValue::as_str),
                entry.get("dataset").and_then(MjValue::as_str),
            ) else {
                continue;
            };
            thread.push((id.to_string(), dataset.to_string()));
        }
    }

    Ok(ImportedConfig {
        fabric_label: text("fabricLabel"),
        next_node_id: values.get("nextNodeId").and_then(MjValue::as_u64),
        wifi,
        thread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::store::MatterJsStorage;
    use std::path::Path;

    /// A namespace written in the `json` driver's shape, which is the same
    /// context/key model every driver stores.
    fn storage_with(namespaces: &[(&str, &str)]) -> (tempfile::TempDir, MatterJsStorage) {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in namespaces {
            let path = dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("driver.json"), r#"{"kind":"json"}"#).unwrap();
            std::fs::write(path.join("storage.json"), contents).unwrap();
        }
        let storage = MatterJsStorage::open(dir.path()).unwrap();
        (dir, storage)
    }

    fn bytes(hex: &str) -> String {
        format!(
            "\"{{\\\"__object__\\\":\\\"Uint8Array\\\",\\\"__value__\\\":\\\"{}\\\"}}\"",
            hex
        )
    }

    fn big_int(value: &str) -> String {
        format!(
            "\"{{\\\"__object__\\\":\\\"BigInt\\\",\\\"__value__\\\":\\\"{}\\\"}}\"",
            value
        )
    }

    fn key_hex() -> String {
        "11".repeat(32)
    }

    fn server_namespace(extra_credentials: &str, icac: &str) -> String {
        format!(
            r#"{{
                "credentials": {{
                    "fabric": {{
                        "fabricIndex": 1,
                        "fabricId": {fabric_id},
                        "nodeId": {node_id},
                        "rootVendorId": 65521,
                        "rootCert": {root_cert},
                        "operationalCert": {noc},
                        {icac}
                        "identityProtectionKey": {ipk},
                        "keyPair": {{ "privateKey": {op_key}, "publicKey": {pub_key} }},
                        "label": "HomeAssistant"
                    }},
                    "rootCertBytes": {root_cert},
                    "rootCertId": 0,
                    "nextCertificateId": 3
                    {extra}
                }},
                "nodes": {{ "commissionedNodes": [[{node_a}, {{}}], [{node_b}, {{}}]] }},
                "nodes.peer1.endpoints.0.commissioning": {{
                    "peerAddress": {{ "fabricIndex": 1, "nodeId": {node_a} }},
                    "commissionedAt": 1700000000000,
                    "fabricIndexOnPeer": 2,
                    "addresses": [{{ "type": "udp", "ip": "fd11::1", "port": 5540 }}]
                }}
            }}"#,
            fabric_id = big_int("1"),
            node_id = big_int("112233"),
            node_a = big_int("1"),
            node_b = big_int("2"),
            root_cert = bytes("aa01"),
            noc = bytes("bb02"),
            ipk = bytes(&"cc".repeat(16)),
            op_key = bytes(&key_hex()),
            pub_key = bytes(&"04".repeat(65)),
            icac = icac,
            extra = extra_credentials,
        )
    }

    #[test]
    fn printing_what_was_read_never_prints_a_secret() {
        let (_dir, storage) = storage_with(&[
            (
                "server",
                &server_namespace(
                    &format!(
                        ", \"rootKeyPair\": {{ \"privateKey\": {} }}",
                        bytes(&key_hex())
                    ),
                    "",
                ),
            ),
            (
                "config",
                r#"{"values":{"wifiSsid":"home","wifiCredentials":"hunter2","threadDataset":"0e08"}}"#,
            ),
        ]);

        let fabric = read_fabric(find_controller_namespace(&storage, None).unwrap()).unwrap();
        let printed = format!("{:?}", fabric);
        assert!(!printed.contains(&key_hex()), "{}", printed);
        assert!(printed.contains("<redacted>"), "{}", printed);
        assert!(printed.contains("0x0000000000000001"), "{}", printed);

        let config = read_config(&storage).unwrap();
        let printed = format!("{:?}", config);
        assert!(!printed.contains("hunter2"), "{}", printed);
        assert!(!printed.contains("0e08"), "{}", printed);
        assert!(
            printed.contains("home"),
            "an SSID is not a secret: {}",
            printed
        );
    }

    #[test]
    fn a_two_tier_fabric_imports_with_the_root_key_as_issuer() {
        let (_dir, storage) = storage_with(&[(
            "server",
            &server_namespace(
                &format!(
                    ", \"rootKeyPair\": {{ \"privateKey\": {} }}",
                    bytes(&key_hex())
                ),
                "",
            ),
        )]);

        let namespace = find_controller_namespace(&storage, None).unwrap();
        let fabric = read_fabric(namespace).unwrap();

        assert_eq!(fabric.node_id, 112233);
        assert_eq!(fabric.fabric_id, 1);
        assert_eq!(fabric.vendor_id, 0xFFF1);
        assert_eq!(fabric.root_cert, vec![0xaa, 0x01]);
        assert_eq!(fabric.noc, vec![0xbb, 0x02]);
        assert!(fabric.icac.is_empty());
        assert_eq!(fabric.ipk_epoch_key.len(), 16);
        assert_eq!(fabric.operational_key.len(), 32);
        assert!(!fabric.issuer_is_icac);
        assert_eq!(fabric.label, "HomeAssistant");
    }

    #[test]
    fn a_three_tier_fabric_issues_through_the_intermediate_key() {
        let (_dir, storage) = storage_with(&[(
            "server",
            &server_namespace(
                &format!(
                    ", \"rootKeyPair\": {{ \"privateKey\": {} }}, \"icacKeyPair\": {{ \"privateKey\": {} }}",
                    bytes(&key_hex()),
                    bytes(&"22".repeat(32))
                ),
                &format!("\"intermediateCACert\": {},", bytes("cc03")),
            ),
        )]);

        let fabric = read_fabric(find_controller_namespace(&storage, None).unwrap()).unwrap();
        assert_eq!(fabric.icac, vec![0xcc, 0x03]);
        assert!(fabric.issuer_is_icac);
        assert_eq!(fabric.issuer_key, vec![0x22; 32]);
    }

    #[test]
    fn an_intermediate_certificate_without_its_key_is_refused() {
        let (_dir, storage) = storage_with(&[(
            "server",
            &server_namespace(
                &format!(
                    ", \"rootKeyPair\": {{ \"privateKey\": {} }}",
                    bytes(&key_hex())
                ),
                &format!("\"intermediateCACert\": {},", bytes("cc03")),
            ),
        )]);

        let error = read_fabric(find_controller_namespace(&storage, None).unwrap())
            .unwrap_err()
            .to_string();
        assert!(error.contains("intermediate CA"), "{}", error);
    }

    #[test]
    fn nodes_come_from_the_list_and_the_per_peer_state() {
        let (_dir, storage) = storage_with(&[(
            "server",
            &server_namespace(&format!(", \"rootKeyPair\": {}", bytes(&key_hex())), ""),
        )]);

        let nodes = read_nodes(find_controller_namespace(&storage, None).unwrap());
        assert_eq!(nodes.len(), 2, "both listed nodes are imported");

        let first = &nodes[0];
        assert_eq!(first.node_id, 1);
        assert_eq!(first.commissioned_at, Some(1700000000000));
        assert_eq!(first.fabric_index_on_peer, Some(2));
        assert_eq!(first.addresses, vec!["fd11::1".to_string()]);

        // The node with no per-peer context is still imported, bare.
        assert_eq!(nodes[1].node_id, 2);
        assert_eq!(nodes[1].commissioned_at, None);
        assert!(nodes[1].addresses.is_empty());
    }

    #[test]
    fn settings_map_onto_ours_including_the_named_credential_lists() {
        let (_dir, storage) = storage_with(&[
            (
                "server",
                &server_namespace(&format!(", \"rootKeyPair\": {}", bytes(&key_hex())), ""),
            ),
            (
                "config",
                r#"{"values":{
                    "fabricLabel":"Living Room",
                    "nextNodeId":42,
                    "wifiSsid":"home",
                    "wifiCredentials":"hunter2",
                    "threadDataset":"0e08",
                    "additionalWifiCredentials":[{"id":"guest","ssid":"guest-net","credentials":"pw"}],
                    "additionalThreadCredentials":[{"id":"shed","dataset":"0e09"}]
                }}"#,
            ),
        ]);

        let config = read_config(&storage).unwrap();
        assert_eq!(config.fabric_label.as_deref(), Some("Living Room"));
        assert_eq!(config.next_node_id, Some(42));
        assert_eq!(
            config.wifi,
            vec![
                ("default".into(), "home".into(), "hunter2".into()),
                ("guest".into(), "guest-net".into(), "pw".into())
            ]
        );
        assert_eq!(
            config.thread,
            vec![
                ("default".into(), "0e08".into()),
                ("shed".into(), "0e09".into())
            ]
        );
    }

    #[test]
    fn a_multi_fabric_install_names_its_namespaces() {
        let (_dir, storage) = storage_with(&[
            (
                "server-1-fff1",
                &server_namespace(&format!(", \"rootKeyPair\": {}", bytes(&key_hex())), ""),
            ),
            (
                "server-2-fff1",
                &server_namespace(&format!(", \"rootKeyPair\": {}", bytes(&key_hex())), ""),
            ),
        ]);

        let error = find_controller_namespace(&storage, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("server-1-fff1"), "{}", error);
        assert!(error.contains("--import-matterjs-namespace"), "{}", error);

        // Named explicitly, it imports.
        let namespace = find_controller_namespace(&storage, Some("server-2-fff1")).unwrap();
        assert_eq!(namespace.name, "server-2-fff1");
    }

    #[test]
    fn a_single_unnamed_fabric_namespace_is_found_without_help() {
        let (_dir, storage) = storage_with(&[(
            "server-1-fff1",
            &server_namespace(&format!(", \"rootKeyPair\": {}", bytes(&key_hex())), ""),
        )]);
        assert_eq!(
            find_controller_namespace(&storage, None).unwrap().name,
            "server-1-fff1"
        );
    }

    #[test]
    fn storage_with_no_fabric_is_reported_as_such() {
        let (_dir, storage) = storage_with(&[("config", r#"{"values":{"fabricLabel":"x"}}"#)]);
        let error = find_controller_namespace(&storage, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no controller fabric"), "{}", error);
    }

    #[test]
    fn the_node_api_layout_is_read_too() {
        // Newer matter.js keeps fabrics in a list under their own namespace.
        let (_dir, storage) = storage_with(&[(
            "server",
            &format!(
                r#"{{
                    "fabrics": {{ "fabrics": [{{
                        "fabricIndex": 1,
                        "fabricId": {fabric_id},
                        "nodeId": {node_id},
                        "rootVendorId": 65521,
                        "rootCert": {root_cert},
                        "operationalCert": {noc},
                        "identityProtectionKey": {ipk},
                        "keyPair": {{ "privateKey": {op_key} }},
                        "label": "HomeAssistant"
                    }}] }},
                    "certificates": {{ "rootCertBytes": {root_cert}, "rootKeyPair": {op_key} }}
                }}"#,
                fabric_id = big_int("1"),
                node_id = big_int("112233"),
                root_cert = bytes("aa01"),
                noc = bytes("bb02"),
                ipk = bytes(&"cc".repeat(16)),
                op_key = bytes(&key_hex()),
            ),
        )]);

        let fabric = read_fabric(find_controller_namespace(&storage, None).unwrap()).unwrap();
        assert_eq!(fabric.node_id, 112233);
        assert!(!fabric.issuer_is_icac);
    }

    #[test]
    fn the_legacy_per_node_contexts_are_a_fallback_source_of_ids() {
        let (_dir, storage) = storage_with(&[(
            "server",
            &format!(
                r#"{{
                    "credentials": {{
                        "fabric": {{
                            "fabricId": {fabric_id}, "nodeId": {node_id}, "rootVendorId": 65521,
                            "rootCert": {root_cert}, "operationalCert": {noc},
                            "identityProtectionKey": {ipk},
                            "keyPair": {{ "privateKey": {op_key} }}, "label": ""
                        }},
                        "rootCertBytes": {root_cert}, "rootKeyPair": {op_key}
                    }},
                    "node-77.0.29": {{ "__version__": 1 }}
                }}"#,
                fabric_id = big_int("1"),
                node_id = big_int("112233"),
                root_cert = bytes("aa01"),
                noc = bytes("bb02"),
                ipk = bytes(&"cc".repeat(16)),
                op_key = bytes(&key_hex()),
            ),
        )]);

        let nodes = read_nodes(find_controller_namespace(&storage, None).unwrap());
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, 77);
    }

    #[test]
    fn a_path_that_is_not_a_storage_directory_says_so() {
        let error = MatterJsStorage::open(Path::new("/nonexistent/matter"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--storage-path"), "{}", error);
    }
}
