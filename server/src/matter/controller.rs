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

/// Fabric configuration for the controller.
#[derive(Clone, Debug)]
pub struct FabricConfig {
    pub vendor_id: u16,
    pub fabric_id: u64,
    pub node_id: u64,
}

impl Default for FabricConfig {
    fn default() -> Self {
        Self {
            vendor_id: 0xFFF1,
            fabric_id: 1,
            node_id: 112233,
        }
    }
}

const ICAC_PRIVATE_KEY_FILE: &str = "controller-icac-key.bin";

/// Scratch space for the controller's CSR.
///
/// Matches what rs-matter's own tests allocate. A CSR is a little over 200
/// bytes, but the DER-encoded signature inside it varies in length with the
/// random values it contains, so the buffer needs real headroom rather than a
/// size that fits the common case.
const CSR_BUF_LEN: usize = 512;

/// Runtime state that must remain with the non-`Send` Matter object.
///
/// rs-matter persists the fabric certificates and the controller operational
/// key, but the ICAC private key is application-owned signing material. Keep a
/// copy in a separate file so a restarted server can issue device NOCs.
pub struct MatterController {
    pub matter: Matter<'static>,
    pub icac_private_key: CanonPkcSecretKey,
    pub storage_path: PathBuf,
}

/// Initialize (or create) the Matter fabric from persistent storage and load
/// the application-owned ICAC signing key.
pub fn init_controller(storage_path: &str, config: &FabricConfig) -> Result<MatterController> {
    let matter = Matter::new(&TEST_DEV_DET, TEST_DEV_COMM, &TEST_DEV_ATT, 0);

    let storage_path = Path::new(storage_path);
    fs::create_dir_all(storage_path)
        .with_context(|| format!("creating storage directory {}", storage_path.display()))?;

    // Restore fabric state from storage if present.
    let kv_store = DirKvBlobStore::new(storage_path.to_path_buf());
    let kv_access = matter.kv(kv_store);
    matter.startup(kv_access)?;

    let icac_private_key = if !matter.has_fabrics() {
        let key = create_fabric(&matter, config)?;
        persist_fabric(&matter, storage_path)?;
        persist_icac_private_key(storage_path, &key)?;
        log::info!("Created new Matter fabric at {}", storage_path.display());
        key
    } else {
        log::info!(
            "Loaded existing Matter fabric from {}",
            storage_path.display()
        );
        load_icac_private_key(storage_path)?
    };

    Ok(MatterController {
        matter,
        icac_private_key,
        storage_path: storage_path.to_path_buf(),
    })
}

/// Compatibility helper for callers that only need the Matter state.
pub fn init_matter(storage_path: &str, config: &FabricConfig) -> Result<Matter<'static>> {
    Ok(init_controller(storage_path, config)?.matter)
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

fn persist_icac_private_key(storage_path: &Path, key: &CanonPkcSecretKey) -> Result<()> {
    fs::create_dir_all(storage_path)
        .with_context(|| format!("creating storage directory {}", storage_path.display()))?;
    let path = storage_path.join(ICAC_PRIVATE_KEY_FILE);
    fs::write(&path, key.access())
        .with_context(|| format!("writing ICAC private key {}", path.display()))?;
    Ok(())
}

fn load_icac_private_key(storage_path: &Path) -> Result<CanonPkcSecretKey> {
    let path = storage_path.join(ICAC_PRIVATE_KEY_FILE);
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "reading ICAC private key {}; existing fabrics created by an older build must be reset",
            path.display()
        )
    })?;
    CanonPkcSecretKey::try_from(bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid ICAC private key {}: {:?}", path.display(), e))
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
