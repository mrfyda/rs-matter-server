//! Lookups against the Distributed Compliance Ledger.
//!
//! The DCL is the CSA's public registry of certified Matter products, the
//! firmware images published for them, and the vendors who make them. It is
//! the only way to know whether a Matter OTA update exists for a device — and
//! it is deliberately separate from a vendor's own update channel, so a product
//! can have newer firmware available through the vendor's app while having no
//! newer *Matter* image here.
//!
//! It is also the registry a vendor id is assigned in, so it can name a vendor
//! that shipped after the reference's static table was last cut.
//!
//! The ledger publishes where an image is and what its digest is, not the
//! image: serving one means fetching it from the vendor's CDN and checking it
//! against that digest. `download_image` does exactly that, and refuses to
//! hand back bytes that do not match — whatever a device would make of them.

use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::{Deserialize, Deserializer};
use sha2::{Digest, Sha256};

use crate::protocol::error::ApiError;
use crate::protocol::model::{MatterSoftwareVersion, UpdateSource};

/// The CSA's production ledger.
pub const MAIN_NET_URL: &str = "https://on.dcl.csa-iot.org";
/// The test ledger, used by devices with test vendor ids.
pub const TEST_NET_URL: &str = "https://on.test-net.dcl.csa-iot.org";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long fetching an image may take end to end. Images run to megabytes
/// and vendor CDNs are of very different speeds, so this is generous; it is a
/// bound on a stalled transfer, not a budget.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// `otaChecksumType` 1: SHA-256 over the whole file. The other values the
/// specification allows are truncated SHA-256 and SHA-3 variants, which nothing
/// in the ledger uses; an image published with one is refused rather than
/// served unchecked.
const CHECKSUM_SHA256: u32 = 1;
/// How long a lookup is reused. Clients poll for updates on a timer, and the
/// ledger changes at the pace of firmware releases, so this keeps a
/// once-a-minute client from becoming once-a-minute traffic to the CSA.
const CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Deserialize)]
struct ModelVersionsResponse {
    #[serde(rename = "modelVersions")]
    model_versions: ModelVersions,
}

#[derive(Debug, Deserialize)]
struct ModelVersions {
    #[serde(rename = "softwareVersions", default)]
    software_versions: Vec<u64>,
}

#[derive(Debug, Deserialize)]
struct ModelVersionResponse {
    #[serde(rename = "modelVersion")]
    model_version: ModelVersion,
}

#[derive(Debug, Deserialize)]
struct ModelVersion {
    vid: u16,
    pid: u16,
    #[serde(rename = "softwareVersion")]
    software_version: u64,
    #[serde(rename = "softwareVersionString", default)]
    software_version_string: String,
    #[serde(rename = "softwareVersionValid", default)]
    software_version_valid: bool,
    #[serde(rename = "otaUrl", default)]
    ota_url: String,
    #[serde(rename = "firmwareInformation", default)]
    firmware_information: String,
    #[serde(rename = "minApplicableSoftwareVersion", default)]
    min_applicable_software_version: u64,
    #[serde(rename = "maxApplicableSoftwareVersion", default)]
    max_applicable_software_version: u64,
    #[serde(rename = "releaseNotesUrl", default)]
    release_notes_url: String,
    /// The ledger sends this as a decimal string; zero means unpublished.
    #[serde(rename = "otaFileSize", default, deserialize_with = "lenient_u64")]
    ota_file_size: Option<u64>,
    /// Base64 of the digest, as the ledger stores it.
    #[serde(rename = "otaChecksum", default)]
    ota_checksum: String,
    #[serde(rename = "otaChecksumType", default)]
    ota_checksum_type: u32,
}

/// A number the ledger may send as a JSON number or as a decimal string, and
/// where zero means "not given".
fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        Some(serde_json::Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    }
    .filter(|size| *size > 0))
}

/// Where a published image is, and how to know the bytes that arrive are it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedImage {
    pub version: MatterSoftwareVersion,
    pub url: String,
    /// The size the ledger publishes, when it publishes one.
    pub size: Option<u64>,
    pub sha256: [u8; 32],
}

/// The digest as the ledger encodes it — base64, per the schema — or as the
/// 64 hex characters a hand-entered record sometimes carries instead.
fn decode_digest(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    // Base64 first, but only a 32-byte result counts: 64 hex characters are
    // also well-formed base64, of the wrong length.
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(text) {
        if let Ok(digest) = <[u8; 32]>::try_from(bytes) {
            return Some(digest);
        }
    }
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?
        .try_into()
        .ok()
}

impl ModelVersion {
    /// Whether this published version can actually be applied to a device
    /// running `current_version`.
    fn applies_to(&self, current_version: u64) -> bool {
        self.software_version_valid
            && self.software_version > current_version
            && self.min_applicable_software_version <= current_version
            && self.max_applicable_software_version >= current_version
            // A version with no image published cannot be delivered to
            // anything, so it is not an available update.
            && !self.ota_url.is_empty()
    }

    /// The image this version publishes, or `None` when it publishes none.
    ///
    /// An image whose digest cannot be checked here is an error rather than
    /// `None`: it exists, and the caller should say why it will not be served.
    fn published_image(self, source: UpdateSource) -> Result<Option<PublishedImage>, ApiError> {
        if self.ota_url.is_empty() {
            return Ok(None);
        }
        if self.ota_checksum_type != CHECKSUM_SHA256 {
            return Err(ApiError::update(format!(
                "The ledger publishes version {} with checksum type {}, and only SHA-256 \
                 (type 1) can be verified here; refusing to serve an unverified image",
                self.software_version, self.ota_checksum_type
            )));
        }
        let sha256 = decode_digest(&self.ota_checksum).ok_or_else(|| {
            ApiError::update(format!(
                "The ledger's checksum for version {} is not a SHA-256 digest; refusing to \
                 serve an unverified image",
                self.software_version
            ))
        })?;
        Ok(Some(PublishedImage {
            url: self.ota_url.clone(),
            size: self.ota_file_size,
            sha256,
            version: self.into_wire(source),
        }))
    }

    fn into_wire(self, source: UpdateSource) -> MatterSoftwareVersion {
        let version_string = if self.software_version_string.is_empty() {
            self.software_version.to_string()
        } else {
            self.software_version_string
        };
        MatterSoftwareVersion {
            vid: self.vid,
            pid: self.pid,
            software_version: self.software_version,
            software_version_string: version_string,
            firmware_information: Some(self.firmware_information).filter(|value| !value.is_empty()),
            min_applicable_software_version: self.min_applicable_software_version,
            max_applicable_software_version: self.max_applicable_software_version,
            release_notes_url: Some(self.release_notes_url).filter(|value| !value.is_empty()),
            update_source: source,
        }
    }
}

/// A DCL client with a short-lived result cache.
pub struct DclClient {
    base_url: String,
    source: UpdateSource,
    cache: Mutex<Vec<CacheEntry>>,
    vendors: Mutex<Vec<VendorEntry>>,
}

#[derive(Debug, Deserialize)]
struct VendorInfoResponse {
    #[serde(rename = "vendorInfo")]
    vendor_info: VendorInfo,
}

#[derive(Debug, Deserialize)]
struct VendorInfo {
    #[serde(rename = "vendorName", default)]
    vendor_name: String,
}

struct VendorEntry {
    vendor_id: u16,
    fetched_at: Instant,
    /// `None` records a vendor the ledger does not know, so a client asking
    /// about it repeatedly does not re-ask the CSA every time.
    name: Option<String>,
}

struct CacheEntry {
    vid: u16,
    pid: u16,
    current_version: u64,
    fetched_at: Instant,
    result: Option<MatterSoftwareVersion>,
}

impl DclClient {
    pub fn main_net() -> Self {
        Self::new(MAIN_NET_URL, UpdateSource::MainNetDcl)
    }

    pub fn test_net() -> Self {
        Self::new(TEST_NET_URL, UpdateSource::TestNetDcl)
    }

    pub fn new(base_url: impl Into<String>, source: UpdateSource) -> Self {
        Self {
            base_url: base_url.into(),
            source,
            cache: Mutex::new(Vec::new()),
            vendors: Mutex::new(Vec::new()),
        }
    }

    /// The name the ledger has for a vendor id, if it has one.
    ///
    /// A vendor the ledger does not know is `Ok(None)`, and so is a ledger
    /// that cannot be reached: naming a vendor is decoration, and a client
    /// asking for 40 of them should not lose the 39 that are known because
    /// one lookup timed out. The failure is logged rather than returned.
    ///
    /// This performs blocking HTTP, so it must be called from a connection
    /// thread and never from the Matter executor.
    pub fn vendor_name(&self, vendor_id: u16) -> Option<String> {
        if let Some(cached) = self.cached_vendor(vendor_id) {
            return cached;
        }
        let name = match self.fetch_vendor_name(vendor_id) {
            Ok(name) => name,
            Err(error) => {
                log::debug!("Vendor {} could not be looked up: {}", vendor_id, error);
                return None;
            }
        };
        let mut vendors = self.vendors.lock().unwrap();
        vendors.retain(|entry| entry.vendor_id != vendor_id);
        vendors.push(VendorEntry {
            vendor_id,
            fetched_at: Instant::now(),
            name: name.clone(),
        });
        name
    }

    fn cached_vendor(&self, vendor_id: u16) -> Option<Option<String>> {
        let mut vendors = self.vendors.lock().unwrap();
        vendors.retain(|entry| entry.fetched_at.elapsed() < CACHE_TTL);
        vendors
            .iter()
            .find(|entry| entry.vendor_id == vendor_id)
            .map(|entry| entry.name.clone())
    }

    fn fetch_vendor_name(&self, vendor_id: u16) -> Result<Option<String>, ApiError> {
        let url = format!("{}/dcl/vendorinfo/vendors/{}", self.base_url, vendor_id);
        match ureq::get(&url).timeout(REQUEST_TIMEOUT).call() {
            Ok(response) => {
                let parsed: VendorInfoResponse = response.into_json().map_err(|error| {
                    ApiError::sdk(format!("The vendor registry replied unreadably: {}", error))
                })?;
                Ok(Some(parsed.vendor_info.vendor_name).filter(|name| !name.is_empty()))
            }
            // An unassigned vendor id is not an error; nobody has one.
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(error) => Err(ApiError::sdk(format!(
                "Could not reach the vendor registry: {}",
                error
            ))),
        }
    }

    /// The newest applicable published firmware, or `None` when the product is
    /// already current.
    ///
    /// This performs blocking HTTP, so it must be called from a connection
    /// thread and never from the Matter executor.
    pub fn check_update(
        &self,
        vid: u16,
        pid: u16,
        current_version: u64,
    ) -> Result<Option<MatterSoftwareVersion>, ApiError> {
        if let Some(cached) = self.cached(vid, pid, current_version) {
            return Ok(cached);
        }
        let result = self.fetch(vid, pid, current_version)?;
        self.store(vid, pid, current_version, result.clone());
        Ok(result)
    }

    fn cached(
        &self,
        vid: u16,
        pid: u16,
        current_version: u64,
    ) -> Option<Option<MatterSoftwareVersion>> {
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|entry| entry.fetched_at.elapsed() < CACHE_TTL);
        cache
            .iter()
            .find(|entry| {
                entry.vid == vid && entry.pid == pid && entry.current_version == current_version
            })
            .map(|entry| entry.result.clone())
    }

    fn store(
        &self,
        vid: u16,
        pid: u16,
        current_version: u64,
        result: Option<MatterSoftwareVersion>,
    ) {
        let mut cache = self.cache.lock().unwrap();
        cache.retain(|entry| {
            entry.vid != vid || entry.pid != pid || entry.current_version != current_version
        });
        cache.push(CacheEntry {
            vid,
            pid,
            current_version,
            fetched_at: Instant::now(),
            result,
        });
    }

    fn fetch(
        &self,
        vid: u16,
        pid: u16,
        current_version: u64,
    ) -> Result<Option<MatterSoftwareVersion>, ApiError> {
        let versions = self.software_versions(vid, pid)?;

        // Try newest first and stop at the first one that actually applies;
        // the newest published version is not necessarily reachable from the
        // version a device is on.
        let mut candidates: Vec<u64> = versions
            .into_iter()
            .filter(|version| *version > current_version)
            .collect();
        candidates.sort_unstable_by(|a, b| b.cmp(a));

        for candidate in candidates {
            let Some(model) = self.model_version(vid, pid, candidate)? else {
                continue;
            };
            if model.applies_to(current_version) {
                return Ok(Some(model.into_wire(self.source)));
            }
        }
        Ok(None)
    }

    /// The image the ledger publishes for one specific version, if that version
    /// applies to a device running `current_version` and comes with an image.
    ///
    /// Not cached: this is asked once, when an update is actually being
    /// applied, and it precedes a download that dwarfs it.
    ///
    /// This performs blocking HTTP, so it must be called from a connection
    /// thread and never from the Matter executor.
    pub fn published_image(
        &self,
        vid: u16,
        pid: u16,
        software_version: u64,
        current_version: u64,
    ) -> Result<Option<PublishedImage>, ApiError> {
        let Some(model) = self.model_version(vid, pid, software_version)? else {
            return Ok(None);
        };
        if !model.applies_to(current_version) {
            return Ok(None);
        }
        model.published_image(self.source)
    }

    fn software_versions(&self, vid: u16, pid: u16) -> Result<Vec<u64>, ApiError> {
        let url = format!("{}/dcl/model/versions/{}/{}", self.base_url, vid, pid);
        match ureq::get(&url).timeout(REQUEST_TIMEOUT).call() {
            Ok(response) => {
                let parsed: ModelVersionsResponse = response.into_json().map_err(|error| {
                    ApiError::update_check(format!(
                        "The update registry replied unreadably: {}",
                        error
                    ))
                })?;
                Ok(parsed.model_versions.software_versions)
            }
            // A product with nothing published is not an error; it simply has
            // no Matter firmware in the registry.
            Err(ureq::Error::Status(404, _)) => Ok(Vec::new()),
            Err(error) => Err(ApiError::update_check(format!(
                "Could not reach the update registry: {}",
                error
            ))),
        }
    }

    /// One published version; `None` when the ledger has no such entry.
    fn model_version(
        &self,
        vid: u16,
        pid: u16,
        software_version: u64,
    ) -> Result<Option<ModelVersion>, ApiError> {
        let url = format!(
            "{}/dcl/model/versions/{}/{}/{}",
            self.base_url, vid, pid, software_version
        );
        let response = match ureq::get(&url).timeout(REQUEST_TIMEOUT).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => {
                return Err(ApiError::update_check(format!(
                    "Could not reach the update registry: {}",
                    error
                )))
            }
        };
        let parsed: ModelVersionResponse = response.into_json().map_err(|error| {
            ApiError::update_check(format!("The update registry replied unreadably: {}", error))
        })?;
        Ok(Some(parsed.model_version))
    }
}

/// Fetch a published image and prove it is the one the ledger describes.
///
/// The bytes come back only if they hash to the ledger's digest; a mismatch,
/// a truncated transfer, or a body longer than `max_size` is an error, and
/// nothing is kept. `max_size` bounds memory, because the caller holds the
/// image whole for the life of the process — a mismatch there is refused
/// before a byte is read when the ledger publishes the size, and as soon as the
/// limit is crossed when it does not.
///
/// This performs blocking HTTP, so it must be called from a connection thread
/// and never from the Matter executor.
pub fn download_image(image: &PublishedImage, max_size: u64) -> Result<Vec<u8>, ApiError> {
    if let Some(size) = image.size {
        if size > max_size {
            return Err(ApiError::update(format!(
                "The ledger says the image is {} bytes, above this server's {} byte limit",
                size, max_size
            )));
        }
    }

    let response = ureq::get(&image.url)
        .timeout(DOWNLOAD_TIMEOUT)
        .call()
        .map_err(|error| {
            ApiError::update(format!(
                "Could not fetch the image from {}: {}",
                image.url, error
            ))
        })?;

    let mut bytes = Vec::new();
    // One byte past the limit is enough to know the limit was crossed.
    response
        .into_reader()
        .take(max_size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ApiError::update(format!(
                "The image could not be read from {}: {}",
                image.url, error
            ))
        })?;
    if bytes.len() as u64 > max_size {
        return Err(ApiError::update(format!(
            "The image from {} is larger than this server's {} byte limit",
            image.url, max_size
        )));
    }
    if let Some(size) = image.size {
        if bytes.len() as u64 != size {
            return Err(ApiError::update(format!(
                "The image from {} is {} bytes, but the ledger says {}",
                image.url,
                bytes.len(),
                size
            )));
        }
    }

    let digest = Sha256::digest(&bytes);
    if digest[..] != image.sha256[..] {
        return Err(ApiError::update(format!(
            "The image from {} does not match the digest the ledger publishes; refusing to \
             serve it",
            image.url
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(software_version: u64, ota_url: &str, min: u64, max: u64) -> ModelVersion {
        ModelVersion {
            vid: 5264,
            pid: 1,
            software_version,
            software_version_string: format!("v{}", software_version),
            software_version_valid: true,
            ota_url: ota_url.to_string(),
            firmware_information: String::new(),
            min_applicable_software_version: min,
            max_applicable_software_version: max,
            release_notes_url: String::new(),
            ota_file_size: None,
            ota_checksum: String::new(),
            ota_checksum_type: 0,
        }
    }

    /// A captured `/dcl/model/versions/<vid>/<pid>/<version>` answer for a
    /// product that publishes an image: what the fields look like on the wire,
    /// the size as a string included.
    const PUBLISHED: &str = r#"{"modelVersion":{"vid":4447,"pid":8194,"softwareVersion":1020,
        "softwareVersionString":"1.0.2.0","cdVersionNumber":1,"firmwareInformation":"",
        "softwareVersionValid":true,
        "otaUrl":"https://cdn.example.test/aqara.matter.4447_8194_1.0.2.0.ota",
        "otaFileSize":"615200","otaChecksum":"udgppdjbZz7SaWPUoFp9us9Isl/AsThTZrSzbRUTXQo=",
        "otaChecksumType":1,"minApplicableSoftwareVersion":1011,
        "maxApplicableSoftwareVersion":1020,"releaseNotesUrl":"","creator":"cosmos1q",
        "schemaVersion":0,"specificationVersion":0}}"#;

    /// Serve one HTTP response on a local port and hand back its URL.
    fn serve_once(status: &str, body: Vec<u8>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/image.ota", listener.local_addr().unwrap());
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                status,
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        });
        url
    }

    fn published(url: &str, size: Option<u64>, bytes: &[u8]) -> PublishedImage {
        PublishedImage {
            version: model(20, url, 1, 100).into_wire(UpdateSource::MainNetDcl),
            url: url.to_string(),
            size,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    #[test]
    fn a_ledger_entry_with_an_image_yields_its_url_size_and_digest() {
        let parsed: ModelVersionResponse = serde_json::from_str(PUBLISHED).unwrap();
        let image = parsed
            .model_version
            .published_image(UpdateSource::MainNetDcl)
            .unwrap()
            .expect("an image is published");
        assert_eq!(
            image.url,
            "https://cdn.example.test/aqara.matter.4447_8194_1.0.2.0.ota"
        );
        assert_eq!(image.size, Some(615200));
        assert_eq!(image.sha256[..4], [0xb9, 0xd8, 0x29, 0xa5]);
        assert_eq!(image.version.software_version, 1020);
        assert_eq!(image.version.update_source, UpdateSource::MainNetDcl);
    }

    #[test]
    fn a_version_without_an_image_publishes_none() {
        let none = model(20, "", 1, 100)
            .published_image(UpdateSource::MainNetDcl)
            .unwrap();
        assert!(none.is_none());
    }

    #[test]
    fn an_unverifiable_checksum_is_refused_rather_than_ignored() {
        let mut other_type = model(20, "https://example.test/image.ota", 1, 100);
        other_type.ota_checksum = "udgppdjbZz7SaWPUoFp9us9Isl/AsThTZrSzbRUTXQo=".into();
        other_type.ota_checksum_type = 7;
        let error = other_type
            .published_image(UpdateSource::MainNetDcl)
            .unwrap_err();
        assert_eq!(error.code.as_i64(), 11);
        assert!(error.details.contains("checksum type 7"));

        let mut garbage = model(20, "https://example.test/image.ota", 1, 100);
        garbage.ota_checksum = "not a digest".into();
        garbage.ota_checksum_type = 1;
        assert!(garbage.published_image(UpdateSource::MainNetDcl).is_err());
    }

    #[test]
    fn a_hex_digest_is_accepted_beside_the_base64_one() {
        let hex =
            "b9d829a5d8db673ed26963d4a05a7dbacf48b25fc0b138536 6b4b36d15135d0a".replace(' ', "");
        let mut version = model(20, "https://example.test/image.ota", 1, 100);
        version.ota_checksum = hex;
        version.ota_checksum_type = 1;
        let image = version
            .published_image(UpdateSource::MainNetDcl)
            .unwrap()
            .unwrap();
        assert_eq!(image.sha256[..4], [0xb9, 0xd8, 0x29, 0xa5]);
        assert_eq!(image.sha256[31], 0x0a);
    }

    #[test]
    fn a_download_that_matches_the_digest_is_returned_whole() {
        let bytes: Vec<u8> = (0..4000u32).map(|i| (i % 251) as u8).collect();
        let url = serve_once("200 OK", bytes.clone());
        let image = published(&url, Some(bytes.len() as u64), &bytes);
        assert_eq!(download_image(&image, 1 << 20).unwrap(), bytes);
    }

    #[test]
    fn a_download_that_does_not_match_the_digest_is_refused() {
        let bytes = b"firmware that was tampered with".to_vec();
        let url = serve_once("200 OK", bytes.clone());
        let mut image = published(&url, None, &bytes);
        image.sha256[0] ^= 0xff;
        let error = download_image(&image, 1 << 20).unwrap_err();
        assert_eq!(error.code.as_i64(), 11);
        assert!(error.details.contains("digest"), "{}", error.details);
    }

    #[test]
    fn a_download_of_the_wrong_size_is_refused_before_it_is_hashed() {
        let bytes = b"short".to_vec();
        let url = serve_once("200 OK", bytes.clone());
        let image = published(&url, Some(999), &bytes);
        let error = download_image(&image, 1 << 20).unwrap_err();
        assert!(
            error.details.contains("the ledger says 999"),
            "{}",
            error.details
        );
    }

    #[test]
    fn a_download_past_the_size_limit_is_refused() {
        let bytes = vec![7u8; 100];
        // Published as larger than the limit: refused without a request.
        let image = published("http://127.0.0.1:9/never", Some(200), &bytes);
        assert!(download_image(&image, 150)
            .unwrap_err()
            .details
            .contains("limit"));
        // Not published: refused once the limit is crossed.
        let url = serve_once("200 OK", bytes.clone());
        let image = published(&url, None, &bytes);
        assert!(download_image(&image, 50)
            .unwrap_err()
            .details
            .contains("limit"));
    }

    #[test]
    fn an_unreachable_image_names_its_url() {
        let url = serve_once("404 Not Found", b"gone".to_vec());
        let image = published(&url, None, b"whatever");
        let error = download_image(&image, 1 << 20).unwrap_err();
        assert_eq!(error.code.as_i64(), 11);
        assert!(error.details.contains(&url), "{}", error.details);
    }

    #[test]
    fn a_published_version_with_no_image_is_not_an_update() {
        // This is exactly what the ledger holds for the Shelly Plug S Gen3:
        // one entry, no OTA image.
        let published = model(16908353, "", 1, 90_000_000);
        assert!(!published.applies_to(16908352));
    }

    #[test]
    fn only_newer_applicable_versions_count() {
        let published = model(20, "https://example.test/image.ota", 10, 30);
        assert!(published.applies_to(15));
        // Not newer than what the device runs.
        assert!(!published.applies_to(20));
        assert!(!published.applies_to(25));
        // Outside the applicable range.
        assert!(!published.applies_to(5));
        assert!(!published.applies_to(31));
    }

    #[test]
    fn an_invalidated_version_is_never_offered() {
        let mut published = model(20, "https://example.test/image.ota", 1, 100);
        published.software_version_valid = false;
        assert!(!published.applies_to(10));
    }

    #[test]
    fn the_wire_shape_omits_empty_optional_fields() {
        let wire =
            model(20, "https://example.test/image.ota", 1, 100).into_wire(UpdateSource::MainNetDcl);
        let rendered = serde_json::to_value(&wire).unwrap();
        assert_eq!(rendered["software_version"], serde_json::json!(20));
        assert_eq!(
            rendered["software_version_string"],
            serde_json::json!("v20")
        );
        assert_eq!(rendered["update_source"], serde_json::json!("main-net-dcl"));
        assert!(rendered.get("release_notes_url").is_none());
        assert!(rendered.get("firmware_information").is_none());
    }

    #[test]
    fn results_are_cached_per_product_and_current_version() {
        let client = DclClient::new("https://example.invalid", UpdateSource::MainNetDcl);
        assert!(client.cached(1, 2, 3).is_none());
        client.store(1, 2, 3, None);
        assert_eq!(client.cached(1, 2, 3), Some(None));
        // A device on a different version is a different question.
        assert!(client.cached(1, 2, 4).is_none());
    }

    /// A captured `/dcl/vendorinfo/vendors/<vid>` answer, so a change to the
    /// shape the ledger sends is caught here rather than in production.
    #[test]
    fn a_vendor_info_answer_yields_the_vendor_name() {
        let body = r#"{"vendorInfo":{"vendorID":5264,"vendorName":"Shelly",
            "companyLegalName":"Shelly Europe Ltd.","companyPreferredName":"Shelly",
            "vendorLandingPageURL":"https://www.shelly.com/","schemaVersion":0}}"#;
        let parsed: VendorInfoResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.vendor_info.vendor_name, "Shelly");
    }

    #[test]
    fn an_unknown_vendor_is_remembered_as_unknown() {
        let client = DclClient::new("https://example.invalid", UpdateSource::MainNetDcl);
        assert!(client.cached_vendor(5264).is_none());
        // Reaching an unreachable ledger caches nothing, so the next caller
        // still gets to try.
        assert_eq!(client.vendor_name(5264), None);
        assert!(client.cached_vendor(5264).is_none());
    }

    /// Hits the real CSA ledger; run with `--ignored` when online.
    #[test]
    #[ignore]
    fn the_ledger_names_a_vendor_by_id() {
        let client = DclClient::main_net();
        assert_eq!(client.vendor_name(5264).as_deref(), Some("Shelly"));
        // Answered from the cache the second time, which is not observable
        // here beyond it still being right.
        assert_eq!(client.vendor_name(5264).as_deref(), Some("Shelly"));
        // 0xFFF4 is a test vendor id, which the production ledger does not
        // carry.
        assert_eq!(client.vendor_name(0xFFF4), None);
    }

    /// Hits the real CSA ledger; run with `--ignored` when online.
    #[test]
    #[ignore]
    fn the_shelly_plug_has_no_matter_update_published() {
        let client = DclClient::main_net();
        // The device reports 16908353 ("1.2.0-s1"), which is also the only
        // version the ledger holds for it.
        assert_eq!(client.check_update(5264, 1, 16908353).unwrap(), None);
    }
}
