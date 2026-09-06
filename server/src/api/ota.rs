//! Firmware updates.
//!
//! Update *discovery* works against the local image store: an image uploaded
//! through `POST /ota-upload/<id>` is matched to a node by the vendor id,
//! product id and software version parsed from its header. Distribution — the
//! OTA Provider cluster and the BDX transfer that follow — is not implemented,
//! so `update_node` reports the documented update error rather than silently
//! doing nothing.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::protocol::error::{ApiError, ApiResult};
use crate::protocol::message::Args;
use crate::protocol::model::{MatterSoftwareVersion, OtaUploadTicket, UpdateSource};

use super::{require_node, CallContext};

/// How long a reserved upload id stays valid.
const UPLOAD_TTL: Duration = Duration::from_secs(60);
/// Default upload size limit (`--ota-upload-max-size-mb`).
pub const MAX_UPLOAD_SIZE: u64 = 64 * 1024 * 1024;
/// How many uploads may be in flight at once.
const MAX_IN_FLIGHT_UPLOADS: usize = 4;

/// One reserved upload slot.
#[derive(Clone, Debug)]
struct Reservation {
    peer: String,
    expires_at: Instant,
}

/// A firmware image held in the local store.
#[derive(Clone, Debug)]
pub struct StoredImage {
    pub version: MatterSoftwareVersion,
    pub bytes: Vec<u8>,
}

/// Upload reservations plus the images they produced.
///
/// A reservation is single use and bound to the client that made it, so a
/// leaked id cannot be redeemed from elsewhere.
pub struct OtaUploadRegistry {
    reservations: Mutex<BTreeMap<String, Reservation>>,
    images: Mutex<Vec<StoredImage>>,
}

impl OtaUploadRegistry {
    pub fn new() -> Self {
        Self {
            reservations: Mutex::new(BTreeMap::new()),
            images: Mutex::new(Vec::new()),
        }
    }

    /// Reserve an id for `peer`.
    pub fn reserve(&self, peer: &str) -> Result<OtaUploadTicket, ApiError> {
        let mut reservations = self.reservations.lock().unwrap();
        let now = Instant::now();
        reservations.retain(|_, reservation| reservation.expires_at > now);
        if reservations.len() >= MAX_IN_FLIGHT_UPLOADS {
            return Err(ApiError::ota_upload(
                "Too many uploads are already in flight; retry once one completes or expires",
            ));
        }

        let upload_id = uuid::Uuid::new_v4().simple().to_string();
        reservations.insert(
            upload_id.clone(),
            Reservation {
                peer: peer.to_string(),
                expires_at: now + UPLOAD_TTL,
            },
        );
        Ok(OtaUploadTicket {
            upload_id,
            expires_in: UPLOAD_TTL.as_secs(),
            max_size: MAX_UPLOAD_SIZE,
        })
    }

    /// Redeem a reservation.
    ///
    /// The id is consumed once it is accepted, whether or not the image that
    /// follows turns out to be valid. A request from the wrong client does not
    /// consume it — otherwise anyone who learned the id could cancel a
    /// legitimate upload.
    pub fn redeem(&self, upload_id: &str, peer: &str) -> Result<(), ApiError> {
        let mut reservations = self.reservations.lock().unwrap();
        let reservation = reservations
            .get(upload_id)
            .cloned()
            .ok_or_else(|| ApiError::ota_upload("Unknown or already-used upload id"))?;

        if reservation.expires_at <= Instant::now() {
            reservations.remove(upload_id);
            return Err(ApiError::ota_upload("The upload id has expired"));
        }
        // Compare only the address, not the ephemeral port: a client may open
        // a separate connection for the upload.
        if host_of(&reservation.peer) != host_of(peer) {
            return Err(ApiError::ota_upload(
                "The upload id was reserved by a different client",
            ));
        }
        reservations.remove(upload_id);
        Ok(())
    }

    pub fn store(&self, image: StoredImage) {
        let mut images = self.images.lock().unwrap();
        images.retain(|existing| {
            existing.version.vid != image.version.vid
                || existing.version.pid != image.version.pid
                || existing.version.software_version != image.version.software_version
        });
        images.push(image);
    }

    /// The newest stored image that applies to a device.
    pub fn best_match(
        &self,
        vendor_id: u16,
        product_id: u16,
        current_version: u64,
    ) -> Option<MatterSoftwareVersion> {
        self.images
            .lock()
            .unwrap()
            .iter()
            .filter(|image| {
                image.version.vid == vendor_id
                    && image.version.pid == product_id
                    && image.version.software_version > current_version
                    && image.version.min_applicable_software_version <= current_version
                    && image.version.max_applicable_software_version >= current_version
            })
            .max_by_key(|image| image.version.software_version)
            .map(|image| image.version.clone())
    }

    pub fn image_count(&self) -> usize {
        self.images.lock().unwrap().len()
    }
}

impl Default for OtaUploadRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn host_of(peer: &str) -> &str {
    // "1.2.3.4:5678" -> "1.2.3.4"; "[fd00::1]:5678" -> "[fd00::1]".
    match peer.rfind(':') {
        Some(index) if !peer[index + 1..].contains(']') => &peer[..index],
        _ => peer,
    }
}

/// Report an available firmware update for a node.
///
/// Locally uploaded images are preferred over the ledger, matching the
/// reference: an operator who uploaded an image meant it to be used.
pub async fn check_node_update(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    if !context.server.runtime.ota_enabled {
        return Err(ApiError::update_check(
            "OTA support is disabled on this server",
        ));
    }

    let node = context
        .server
        .nodes
        .get(node_id)
        .ok_or_else(|| ApiError::node_not_exists(node_id))?;
    let vendor_id = node.attributes.get("0/40/2").and_then(Value::as_u64);
    let product_id = node.attributes.get("0/40/4").and_then(Value::as_u64);
    let software_version = node.attributes.get("0/40/9").and_then(Value::as_u64);

    let (Some(vendor_id), Some(product_id), Some(software_version)) =
        (vendor_id, product_id, software_version)
    else {
        return Err(ApiError::update_check(format!(
            "Node {} has not been interviewed, so its firmware version is unknown",
            node_id
        )));
    };

    if let Some(version) =
        context
            .server
            .ota
            .best_match(vendor_id as u16, product_id as u16, software_version)
    {
        return Ok(serde_json::to_value(version).unwrap_or(Value::Null));
    }

    // Nothing local: ask the Distributed Compliance Ledger, which is the only
    // authority on whether a *Matter* image exists. A vendor's own update
    // channel is separate and invisible here, so a device can be current by
    // this answer while the vendor's app offers something newer.
    match context
        .server
        .check_dcl(vendor_id as u16, product_id as u16, software_version)?
    {
        Some(version) => Ok(serde_json::to_value(version).unwrap_or(Value::Null)),
        // No update is a successful answer, not an error.
        None => Ok(Value::Null),
    }
}

/// Apply a firmware update.
///
/// Distributing an image needs an OTA Provider cluster server and a BDX
/// transfer, which this build does not implement; the documented update error
/// says so rather than reporting a success that will never happen.
pub async fn update_node(args: &Args, context: CallContext<'_>) -> ApiResult {
    let node_id = require_node(args, context)?;
    let _ = args.u64("software_version")?;
    Err(ApiError::update(format!(
        "Node {} cannot be updated: this server has no OTA provider, so it cannot deliver firmware images",
        node_id
    )))
}

/// Reserve an id for an image upload over HTTP.
pub async fn initiate_ota_upload(_args: &Args, context: CallContext<'_>) -> ApiResult {
    if !context.server.runtime.ota_enabled {
        return Err(ApiError::ota_upload(
            "OTA support is disabled on this server",
        ));
    }
    let ticket = context.server.ota.reserve(context.peer)?;
    Ok(serde_json::to_value(ticket).unwrap_or(Value::Null))
}

/// Parse the header of a Matter `.ota` image.
///
/// The file starts with a fixed magic, the total size, a header size, and then
/// a TLV structure carrying the vendor id, product id and software version the
/// image applies to.
pub fn parse_ota_header(bytes: &[u8]) -> Result<MatterSoftwareVersion, ApiError> {
    const OTA_MAGIC: u32 = 0x1BEE_F11E;
    const FIXED_HEADER_LEN: usize = 4 + 8 + 4;

    if bytes.len() < FIXED_HEADER_LEN {
        return Err(ApiError::ota_upload(
            "The image is too short to be an OTA file",
        ));
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    if magic != OTA_MAGIC {
        return Err(ApiError::ota_upload(
            "The image does not start with the Matter OTA file identifier",
        ));
    }
    let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let header_end = FIXED_HEADER_LEN
        .checked_add(header_len)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| ApiError::ota_upload("The OTA header extends past the end of the file"))?;

    let header = rs_matter::tlv::TLVElement::new(&bytes[FIXED_HEADER_LEN..header_end]);
    let decoded = crate::matter::tlv_json::to_json(&header)
        .map_err(|_| ApiError::ota_upload("The OTA header is not valid TLV"))?;

    // OTA header TLV tags: 0 vendorId, 1 productId, 2 softwareVersion,
    // 3 softwareVersionString, 4 payloadSize, 5 minApplicableVersion,
    // 6 maxApplicableVersion, 7 releaseNotesUrl.
    let field = |tag: &str| decoded.get(tag).cloned().unwrap_or(Value::Null);
    let integer = |tag: &str| field(tag).as_u64();

    let vid =
        integer("0").ok_or_else(|| ApiError::ota_upload("The OTA header has no vendor id"))?;
    let pid =
        integer("1").ok_or_else(|| ApiError::ota_upload("The OTA header has no product id"))?;
    let software_version = integer("2")
        .ok_or_else(|| ApiError::ota_upload("The OTA header has no software version"))?;

    Ok(MatterSoftwareVersion {
        vid: vid as u16,
        pid: pid as u16,
        software_version,
        software_version_string: field("3")
            .as_str()
            .unwrap_or(&software_version.to_string())
            .to_string(),
        firmware_information: None,
        min_applicable_software_version: integer("5").unwrap_or(0),
        max_applicable_software_version: integer("6").unwrap_or(u64::MAX),
        release_notes_url: field("7").as_str().map(str::to_string),
        update_source: UpdateSource::Local,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tests_support::{call, test_context};
    use crate::protocol::model::MatterNodeData;
    use crate::storage::StoredNode;
    use futures_lite::future::block_on;
    use serde_json::json;

    fn version(software_version: u64) -> MatterSoftwareVersion {
        MatterSoftwareVersion {
            vid: 0xFFF1,
            pid: 0x8001,
            software_version,
            software_version_string: software_version.to_string(),
            firmware_information: None,
            min_applicable_software_version: 0,
            max_applicable_software_version: u64::MAX,
            release_notes_url: None,
            update_source: UpdateSource::Local,
        }
    }

    #[test]
    fn a_reservation_is_single_use_and_bound_to_its_client() {
        let registry = OtaUploadRegistry::new();
        let ticket = registry.reserve("192.168.1.5:40001").unwrap();
        assert_eq!(ticket.max_size, MAX_UPLOAD_SIZE);
        assert_eq!(ticket.expires_in, 60);

        // A different client cannot redeem it.
        assert!(registry
            .redeem(&ticket.upload_id, "192.168.1.6:40002")
            .is_err());
        // The reserving client can, from a different port.
        assert!(registry
            .redeem(&ticket.upload_id, "192.168.1.5:55555")
            .is_ok());
        // ...but only once.
        assert!(registry
            .redeem(&ticket.upload_id, "192.168.1.5:40001")
            .is_err());
    }

    #[test]
    fn in_flight_uploads_are_capped() {
        let registry = OtaUploadRegistry::new();
        for _ in 0..MAX_IN_FLIGHT_UPLOADS {
            registry.reserve("10.0.0.1:1").unwrap();
        }
        let error = registry.reserve("10.0.0.1:1").unwrap_err();
        assert_eq!(error.code.as_i64(), 101);
    }

    #[test]
    fn the_newest_applicable_image_wins() {
        let registry = OtaUploadRegistry::new();
        registry.store(StoredImage {
            version: version(2),
            bytes: vec![],
        });
        registry.store(StoredImage {
            version: version(5),
            bytes: vec![],
        });
        assert_eq!(
            registry
                .best_match(0xFFF1, 0x8001, 1)
                .unwrap()
                .software_version,
            5
        );
        // Nothing newer than what the device already runs.
        assert!(registry.best_match(0xFFF1, 0x8001, 5).is_none());
        // A different product does not match.
        assert!(registry.best_match(0xFFF1, 0x9999, 1).is_none());
    }

    #[test]
    fn storing_the_same_version_twice_replaces_it() {
        let registry = OtaUploadRegistry::new();
        registry.store(StoredImage {
            version: version(2),
            bytes: vec![1],
        });
        registry.store(StoredImage {
            version: version(2),
            bytes: vec![2],
        });
        assert_eq!(registry.image_count(), 1);
    }

    #[test]
    fn check_node_update_needs_an_interviewed_node() {
        let context = test_context();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        let args = Args::new(json!({ "node_id": 1 }));
        let error = block_on(check_node_update(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 10);
        assert!(error.details.contains("not been interviewed"));
    }

    #[test]
    fn a_locally_uploaded_image_wins_over_the_ledger() {
        // The local store is consulted first, so this resolves without any
        // network access at all.
        let context = test_context();
        let mut node = MatterNodeData::new(1, "2026-01-01T00:00:00.000Z".into());
        node.attributes.insert("0/40/2".into(), json!(0xFFF1));
        node.attributes.insert("0/40/4".into(), json!(0x8001));
        node.attributes.insert("0/40/9".into(), json!(1));
        context.nodes.upsert(StoredNode::new(node));
        context.ota.store(StoredImage {
            version: version(7),
            bytes: vec![],
        });

        let args = Args::new(json!({ "node_id": 1 }));
        let found = block_on(check_node_update(&args, call(&context))).unwrap();
        assert_eq!(found["software_version"], json!(7));
        assert_eq!(found["update_source"], json!("local"));
    }

    #[test]
    fn check_node_update_reports_null_when_no_image_applies() {
        let context = test_context();
        let mut node = MatterNodeData::new(1, "2026-01-01T00:00:00.000Z".into());
        node.attributes.insert("0/40/2".into(), json!(0xFFF1));
        node.attributes.insert("0/40/4".into(), json!(0x8001));
        node.attributes.insert("0/40/9".into(), json!(1));
        context.nodes.upsert(StoredNode::new(node));

        // A test vendor id with the test ledger disabled resolves to "no
        // update" without touching the network.
        let args = Args::new(json!({ "node_id": 1 }));
        assert_eq!(
            block_on(check_node_update(&args, call(&context))).unwrap(),
            Value::Null
        );

        context.ota.store(StoredImage {
            version: version(3),
            bytes: vec![],
        });
        let found = block_on(check_node_update(&args, call(&context))).unwrap();
        assert_eq!(found["software_version"], json!(3));
        assert_eq!(found["update_source"], json!("local"));
    }

    #[test]
    fn update_node_reports_the_documented_update_error() {
        let context = test_context();
        context.nodes.upsert(StoredNode::new(MatterNodeData::new(
            1,
            "2026-01-01T00:00:00.000Z".into(),
        )));
        let args = Args::new(json!({ "node_id": 1, "software_version": 2 }));
        let error = block_on(update_node(&args, call(&context))).unwrap_err();
        assert_eq!(error.code.as_i64(), 11);
    }

    #[test]
    fn a_corrupt_image_is_rejected_with_the_ota_upload_code() {
        let error = parse_ota_header(b"not an ota file").unwrap_err();
        assert_eq!(error.code.as_i64(), 101);
        assert!(parse_ota_header(&[]).is_err());
    }

    #[test]
    fn peer_hosts_compare_without_their_port() {
        assert_eq!(host_of("192.168.1.5:40001"), "192.168.1.5");
        assert_eq!(host_of("[fd00::1]:40001"), "[fd00::1]");
        assert_eq!(host_of("192.168.1.5"), "192.168.1.5");
    }
}
