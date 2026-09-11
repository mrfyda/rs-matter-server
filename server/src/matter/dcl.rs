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

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::protocol::error::ApiError;
use crate::protocol::model::{MatterSoftwareVersion, UpdateSource};

/// The CSA's production ledger.
pub const MAIN_NET_URL: &str = "https://on.dcl.csa-iot.org";
/// The test ledger, used by devices with test vendor ids.
pub const TEST_NET_URL: &str = "https://on.test-net.dcl.csa-iot.org";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
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
            let model = self.model_version(vid, pid, candidate)?;
            if model.applies_to(current_version) {
                return Ok(Some(model.into_wire(self.source)));
            }
        }
        Ok(None)
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

    fn model_version(
        &self,
        vid: u16,
        pid: u16,
        software_version: u64,
    ) -> Result<ModelVersion, ApiError> {
        let url = format!(
            "{}/dcl/model/versions/{}/{}/{}",
            self.base_url, vid, pid, software_version
        );
        let response = ureq::get(&url)
            .timeout(REQUEST_TIMEOUT)
            .call()
            .map_err(|error| {
                ApiError::update_check(format!("Could not reach the update registry: {}", error))
            })?;
        let parsed: ModelVersionResponse = response.into_json().map_err(|error| {
            ApiError::update_check(format!("The update registry replied unreadably: {}", error))
        })?;
        Ok(parsed.model_version)
    }
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
        }
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
