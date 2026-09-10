//! Matter controller — fabric management and commissioning.
//!
//! Wraps rs-matter's fabric state, certificate chain, and commissioner.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand_core::{OsRng, RngCore};
use rs_matter::cert::gen::VALID_FOREVER;
use rs_matter::cert::MAX_CERT_TLV_AND_ASN1_LEN;
use rs_matter::crypto::{
    default_crypto, CanonAeadKey, CanonPkcSecretKey, Crypto, SecretKey, SigningSecretKey,
};
use rs_matter::dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::error::ErrorCode;
use rs_matter::fabric::FabricPersist;
use rs_matter::onboard::cac::{IcacGenerator, RcacGenerator};
use rs_matter::onboard::noc::NocGenerator;
use rs_matter::persist::DirKvBlobStore;
use rs_matter::Matter;

use crate::migrate::ImportedFabric;

/// Fabric configuration for the controller.
#[derive(Clone, Debug)]
pub struct FabricConfig {
    pub vendor_id: u16,
    pub fabric_id: u64,
    pub node_id: u64,
    /// The port this node answers Matter traffic on, and advertises in mDNS.
    ///
    /// A controller is a node others reach — a device fetching a firmware
    /// image, an ICD following up a check-in — and they find it by resolving
    /// its operational instance, which carries this number. It has to be a
    /// port this server is actually listening on, and a stable one: a device
    /// caches what it resolved.
    pub port: u16,
}

/// The port Matter reserves for operational traffic.
pub const MATTER_PORT: u16 = 5540;

impl Default for FabricConfig {
    fn default() -> Self {
        Self {
            vendor_id: 0xFFF1,
            fabric_id: 1,
            node_id: 112233,
            port: MATTER_PORT,
        }
    }
}

/// Where the NOC-issuing key lives.
///
/// The name says ICAC because that is what a fabric created here uses, and
/// installs in the field already have a file by that name. A fabric imported
/// from matterjs-server usually has no ICAC and signs with its root key
/// instead; that key goes in the same file, because what matters to every
/// reader is that this is the key that signs certificates for devices.
const ISSUER_PRIVATE_KEY_FILE: &str = "controller-icac-key.bin";

/// Scratch space for the controller's CSR.
///
/// Matches what rs-matter's own tests allocate. A CSR is a little over 200
/// bytes, but the DER-encoded signature inside it varies in length with the
/// random values it contains, so the buffer needs real headroom rather than a
/// size that fits the common case.
const CSR_BUF_LEN: usize = 512;

/// Where the fabric this server is running on came from.
///
/// The caller needs this to know whether the rest of an import — the node list
/// and the settings — should be written: they belong to the fabric, and
/// writing them over the state of a server that already had one would lose
/// data rather than migrate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FabricOrigin {
    /// Read back from this server's own storage.
    Loaded,
    /// Created here, on a first start with no storage.
    Created,
    /// Installed from a matterjs-server storage directory.
    Imported,
}

/// Runtime state that must remain with the non-`Send` Matter object.
///
/// rs-matter persists the fabric certificates and the controller operational
/// key, but the key that signs NOCs for devices is application-owned. Keep a
/// copy in a separate file so a restarted server can go on commissioning.
pub struct MatterController {
    pub matter: Matter<'static>,
    pub issuer_private_key: CanonPkcSecretKey,
    pub storage_path: PathBuf,
    pub origin: FabricOrigin,
}

/// Initialize (or create) the Matter fabric from persistent storage and load
/// the application-owned signing key.
pub fn init_controller(storage_path: &str, config: &FabricConfig) -> Result<MatterController> {
    init_controller_with_import(storage_path, config, None)
}

/// As [`init_controller`], but seeding a first start from another server's
/// fabric instead of creating one.
///
/// The import is ignored — with a log line, not an error — when storage
/// already holds a fabric. The flag that requests it lives in a compose file
/// or a service unit and will be passed on every start after the first, so
/// refusing to start would turn a successful migration into a boot loop.
pub fn init_controller_with_import(
    storage_path: &str,
    config: &FabricConfig,
    imported: Option<&ImportedFabric>,
) -> Result<MatterController> {
    let matter = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, config.port);

    let storage_path = Path::new(storage_path);
    fs::create_dir_all(storage_path)
        .with_context(|| format!("creating storage directory {}", storage_path.display()))?;

    // Restore fabric state from storage if present.
    let kv_store = DirKvBlobStore::new(storage_path.to_path_buf());
    let kv_access = matter.kv(kv_store);
    matter.startup(kv_access)?;

    let (issuer_private_key, origin) = if matter.has_fabrics() {
        log::info!(
            "Loaded existing Matter fabric from {}",
            storage_path.display()
        );
        if imported.is_some() {
            log::info!(
                "Ignoring the requested matterjs-server import: {} already holds a fabric",
                storage_path.display()
            );
        }
        (load_issuer_private_key(storage_path)?, FabricOrigin::Loaded)
    } else if let Some(imported) = imported {
        let key = install_fabric(&matter, imported)?;
        persist_fabric(&matter, storage_path)?;
        persist_issuer_private_key(storage_path, &key)?;
        log::info!(
            "Imported the matterjs-server fabric (fabric id 0x{:016x}, controller node 0x{:016x}) into {}",
            imported.fabric_id,
            imported.node_id,
            storage_path.display()
        );
        (key, FabricOrigin::Imported)
    } else {
        let key = create_fabric(&matter, config)?;
        persist_fabric(&matter, storage_path)?;
        persist_issuer_private_key(storage_path, &key)?;
        log::info!("Created new Matter fabric at {}", storage_path.display());
        (key, FabricOrigin::Created)
    };

    Ok(MatterController {
        matter,
        issuer_private_key,
        storage_path: storage_path.to_path_buf(),
        origin,
    })
}

/// Compatibility helper for callers that only need the Matter state.
pub fn init_matter(storage_path: &str, config: &FabricConfig) -> Result<Matter<'static>> {
    Ok(init_controller(storage_path, config)?.matter)
}

/// Install a fabric read from another controller's storage.
///
/// Every value comes across unchanged — root certificate, the controller's own
/// NOC and operational key, and the IPK — because that is the whole point: a
/// device recognises its fabric by the root public key and grants admin to a
/// specific controller node id, so an identical identity means already
/// commissioned devices need not be touched. rs-matter re-derives the node id,
/// the fabric id and the compressed fabric id from the certificates, and the
/// operational IPK from the epoch key, so nothing that can be computed is
/// carried over and trusted.
fn install_fabric(matter: &Matter<'_>, imported: &ImportedFabric) -> Result<CanonPkcSecretKey> {
    let crypto = default_crypto(OsRng, rs_matter::dm::devices::test::DAC_PRIVKEY);

    let operational_key = CanonPkcSecretKey::try_from(imported.operational_key.as_slice())
        .map_err(|e| anyhow::anyhow!("the imported operational key is unusable: {:?}", e))?;
    let issuer_key = CanonPkcSecretKey::try_from(imported.issuer_key.as_slice())
        .map_err(|e| anyhow::anyhow!("the imported issuing key is unusable: {:?}", e))?;
    let mut epoch_key = CanonAeadKey::new();
    if imported.ipk_epoch_key.len() != epoch_key.access().len() {
        anyhow::bail!(
            "the imported identity protection key is {} bytes, expected {}",
            imported.ipk_epoch_key.len(),
            epoch_key.access().len()
        );
    }
    epoch_key
        .access_mut()
        .copy_from_slice(&imported.ipk_epoch_key);

    let fab_idx = matter
        .with_state(|state| {
            state
                .fabrics
                .add(
                    &crypto,
                    operational_key.reference(),
                    &imported.root_cert,
                    &imported.noc,
                    &imported.icac,
                    Some(epoch_key.reference()),
                    imported.vendor_id,
                    imported.node_id,
                )
                .map(|fabric| fabric.fab_idx())
        })
        .map_err(|e| anyhow::anyhow!("installing the imported fabric: {:?}", e.code()))?;

    // The identifiers rs-matter derived have to match what the source server
    // was using, or the fabric is not the same fabric and every device on it
    // would refuse us. Checking is cheap; discovering it against a device is
    // not.
    matter
        .with_state(|state| -> Result<(), rs_matter::error::Error> {
            let fabric = state.fabrics.fabric(fab_idx)?;
            if let Some(expected) = imported.compressed_fabric_id {
                if fabric.compressed_fabric_id() != expected {
                    log::error!(
                        "The imported certificates give compressed fabric id 0x{:016x}, but the \
                     source server was announcing 0x{:016x}. Devices look this controller up \
                     by that value, so importing it would leave them unreachable",
                        fabric.compressed_fabric_id(),
                        expected,
                    );
                    return Err(ErrorCode::InvalidData.into());
                }
            }
            if fabric.node_id() != imported.node_id || fabric.fabric_id() != imported.fabric_id {
                log::error!(
                "The imported certificates describe node 0x{:016x} on fabric 0x{:016x}, but the \
                 source server recorded node 0x{:016x} on fabric 0x{:016x}",
                fabric.node_id(),
                fabric.fabric_id(),
                imported.node_id,
                imported.fabric_id,
            );
                return Err(ErrorCode::InvalidData.into());
            }
            Ok(())
        })
        .map_err(|_| {
            anyhow::anyhow!(
                "the imported certificates do not match the fabric they were stored with; \
             the source storage is inconsistent and importing it would not work"
            )
        })?;

    if !imported.label.is_empty() {
        // A label longer than the spec allows is the source server's problem,
        // not a reason to abandon the import.
        if let Err(error) = matter.with_state(|state| {
            state.fabrics.update_label(fab_idx, &imported.label)?;
            Ok::<(), rs_matter::error::Error>(())
        }) {
            log::warn!(
                "Could not carry over the fabric label '{}': {:?}",
                imported.label,
                error.code()
            );
        }
    }

    log::info!(
        "Fabric imported: vendor=0x{:04x}, fabric=0x{:016x}, node=0x{:016x}, NOCs signed by the {}",
        imported.vendor_id,
        imported.fabric_id,
        imported.node_id,
        if imported.issuer_is_icac {
            "intermediate CA"
        } else {
            "root CA"
        }
    );

    Ok(issuer_key)
}

pub fn persist_fabric(matter: &Matter<'_>, storage_path: &Path) -> Result<()> {
    let kv = matter.kv(DirKvBlobStore::new(storage_path.to_path_buf()));
    let mut persist = FabricPersist::new(kv);
    matter.with_state(|state| {
        let fabric = state.fabrics.iter().next().ok_or(ErrorCode::NotFound)?;
        persist.store(fabric)
    })?;
    persist.run()?;
    Ok(())
}

fn persist_issuer_private_key(storage_path: &Path, key: &CanonPkcSecretKey) -> Result<()> {
    fs::create_dir_all(storage_path)
        .with_context(|| format!("creating storage directory {}", storage_path.display()))?;
    let path = storage_path.join(ISSUER_PRIVATE_KEY_FILE);
    fs::write(&path, key.access())
        .with_context(|| format!("writing the issuing private key {}", path.display()))?;
    Ok(())
}

fn load_issuer_private_key(storage_path: &Path) -> Result<CanonPkcSecretKey> {
    let path = storage_path.join(ISSUER_PRIVATE_KEY_FILE);
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "reading the issuing private key {}; existing fabrics created by an older build must be reset",
            path.display()
        )
    })?;
    CanonPkcSecretKey::try_from(bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid issuing private key {}: {:?}", path.display(), e))
}

/// How many times fabric creation is attempted before giving up.
///
/// rs-matter's certificate generators fail with `InvalidData` for a small
/// fraction of randomly generated keys — measured at 16 failures in 3000
/// creations (0.5%), split between the root and intermediate certificates,
/// with every input except the key material held constant. The cause is
/// upstream, most likely a DER encoding case that depends on the signature's
/// random values.
///
/// Retrying is sound because nothing is persisted until the final step: a
/// failed attempt leaves no fabric behind, and the next attempt uses fresh key
/// material. Without this, roughly one first boot in two hundred fails.
const FABRIC_CREATION_ATTEMPTS: usize = 5;

/// Create a new fabric for this controller, retrying past the upstream flake.
fn create_fabric(matter: &Matter<'_>, config: &FabricConfig) -> Result<CanonPkcSecretKey> {
    let mut last_error = None;
    for attempt in 1..=FABRIC_CREATION_ATTEMPTS {
        match create_fabric_once(matter, config) {
            Ok(key) => {
                if attempt > 1 {
                    log::info!("Fabric created on attempt {attempt}");
                }
                return Ok(key);
            }
            Err(error) => {
                log::warn!("Fabric creation attempt {attempt} failed: {error:#}");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("fabric creation failed")))
}

fn create_fabric_once(matter: &Matter<'_>, config: &FabricConfig) -> Result<CanonPkcSecretKey> {
    // Use the test DAC (Device Attestation Certificate) for development.
    // In production, this should be a vendor-specific DAC.
    let crypto = default_crypto(OsRng, rs_matter::dm::devices::test::DAC_PRIVKEY);

    // Step 1: Generate RCAC (Root CA).
    let mut rcac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut rcac_gen = RcacGenerator::new(&mut rcac_buf);
    let (rcac_priv, rcac) = rcac_gen
        .generate(&crypto, config.fabric_id, VALID_FOREVER)
        .map_err(|e| anyhow::anyhow!("generating the root certificate: {:?}", e.code()))?;

    // Step 2: Generate ICAC (Intermediate CA).
    let mut icac_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut icac_gen = IcacGenerator::new(&mut icac_buf);
    let (icac_priv, icac) = icac_gen
        .generate(&crypto, rcac_priv.reference(), rcac, VALID_FOREVER)
        .map_err(|e| anyhow::anyhow!("generating the intermediate certificate: {:?}", e.code()))?;
    drop(rcac_priv); // RCAC private key would be HSM-stored in production.

    // Step 3: Controller operational keypair + CSR.
    let controller_key = crypto
        .generate_secret_key()
        .map_err(|e| anyhow::anyhow!("generating the operational key: {:?}", e.code()))?;
    let mut csr_buf = [0u8; CSR_BUF_LEN];
    let csr = controller_key
        .csr(&mut csr_buf)
        .map_err(|e| anyhow::anyhow!("building the CSR: {:?}", e.code()))?;
    let mut controller_key_canon = CanonPkcSecretKey::new();
    controller_key
        .write_canon(&mut controller_key_canon)
        .map_err(|e| anyhow::anyhow!("encoding the operational key: {:?}", e.code()))?;

    // Step 4: Generate controller NOC.
    let mut noc_buf = [0u8; MAX_CERT_TLV_AND_ASN1_LEN];
    let mut noc_gen = NocGenerator::create(icac_priv.reference(), rcac, icac, &mut noc_buf)
        .map_err(|e| anyhow::anyhow!("preparing NOC issuance: {:?}", e.code()))?;
    let noc = noc_gen
        .generate(&crypto, csr, config.node_id, &[], VALID_FOREVER)
        .map_err(|e| anyhow::anyhow!("issuing the controller NOC: {:?}", e.code()))?;

    // Step 5: Generate IPK (16 random bytes).
    let mut ipk = CanonAeadKey::new();
    {
        let mut rand = crypto
            .rand()
            .map_err(|e| anyhow::anyhow!("obtaining randomness for the IPK: {:?}", e.code()))?;
        rand.fill_bytes(ipk.access_mut());
    }

    // Step 6: Install fabric (extract fab_idx to avoid returning a reference).
    matter
        .with_state(|state| {
            state
                .fabrics
                .add(
                    &crypto,
                    controller_key_canon.reference(),
                    rcac,
                    noc,
                    icac,
                    Some(ipk.reference()),
                    config.vendor_id,
                    config.node_id,
                )
                .map(|f| f.fab_idx())
        })
        .map_err(|e| anyhow::anyhow!("installing the fabric: {:?}", e.code()))?;

    log::info!(
        "Fabric created: vendor=0x{:04x}, fabric={}, node=0x{:016x}",
        config.vendor_id,
        config.fabric_id,
        config.node_id,
    );

    Ok(icac_priv)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CSR buffer must fit every CSR, not merely the average one.
    ///
    /// A CSR carries a DER-encoded ECDSA signature whose length depends on the
    /// random `r`/`s` values: leading zero bytes are stripped, so successive
    /// CSRs from the same key size differ by a byte or two. A buffer sized for
    /// the typical case therefore fails intermittently — which is exactly how
    /// this surfaced, passing locally and failing once in CI.
    #[test]
    fn csr_buffer_fits_every_signature_length() {
        let crypto = default_crypto(OsRng, rs_matter::dm::devices::test::DAC_PRIVKEY);
        let mut longest = 0;
        for _ in 0..200 {
            let key = crypto.generate_secret_key().expect("key");
            let mut buf = [0u8; CSR_BUF_LEN];
            let csr = key.csr(&mut buf).expect("the CSR must fit the buffer");
            longest = longest.max(csr.len());
        }
        assert!(
            longest < CSR_BUF_LEN,
            "longest CSR was {longest} bytes in a {CSR_BUF_LEN}-byte buffer"
        );
        // Headroom, so a future encoding change does not silently start
        // failing one run in a hundred.
        assert!(
            CSR_BUF_LEN - longest >= 64,
            "only {} bytes of headroom above the longest CSR ({longest})",
            CSR_BUF_LEN - longest
        );
    }
}
