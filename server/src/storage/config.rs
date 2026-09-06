//! Persistent controller configuration: fabric label, node-id counter, and
//! commissioning credentials.
//!
//! Credentials are stored as named lists with a reserved `default` entry, and
//! secrets are write-only: [`CredentialSummaries`] is the only view that leaves
//! this module, and it carries no passwords or datasets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::protocol::model::{
    AllCredentialsSummary, ThreadCredentialSummary, WifiCredentialSummary,
};

pub const DEFAULT_CREDENTIAL_ID: &str = "default";
const RESERVED_CREDENTIAL_IDS: &[&str] = &["default", "delete"];
pub const DEFAULT_FABRIC_LABEL: &str = "HomeAssistant";
pub const MAX_FABRIC_LABEL_LEN: usize = 32;

/// Reject a credential id, or canonicalise it to `default`.
///
/// Ids are compared case-insensitively but stored as the caller wrote them, so
/// a list cannot hold two entries that differ only in case.
fn validate_credential_id(id: &str) -> Result<String, String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return Err("invalid-credential-id: id must be non-empty".into());
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == DEFAULT_CREDENTIAL_ID {
        return Ok(DEFAULT_CREDENTIAL_ID.to_string());
    }
    if RESERVED_CREDENTIAL_IDS.contains(&lower.as_str()) {
        return Err(format!("invalid-credential-id: '{}' is reserved", trimmed));
    }
    Ok(trimmed.to_string())
}

fn same_id(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WifiEntry {
    id: String,
    ssid: String,
    credentials: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ThreadEntry {
    id: String,
    dataset: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConfigData {
    #[serde(default = "default_fabric_label")]
    fabric_label: String,
    #[serde(default = "default_next_node_id")]
    next_node_id: u64,
    #[serde(default)]
    wifi_ssid: Option<String>,
    #[serde(default)]
    wifi_credentials: Option<String>,
    #[serde(default)]
    thread_dataset: Option<String>,
    #[serde(default)]
    additional_wifi: Vec<WifiEntry>,
    #[serde(default)]
    additional_thread: Vec<ThreadEntry>,
}

fn default_fabric_label() -> String {
    DEFAULT_FABRIC_LABEL.to_string()
}

fn default_next_node_id() -> u64 {
    1
}

/// Thread-safe, optionally file-backed controller configuration.
pub struct ConfigStore {
    data: std::sync::Mutex<ConfigData>,
    path: Option<PathBuf>,
}

impl ConfigStore {
    pub fn in_memory() -> Self {
        Self {
            data: std::sync::Mutex::new(ConfigData {
                fabric_label: default_fabric_label(),
                next_node_id: default_next_node_id(),
                ..ConfigData::default()
            }),
            path: None,
        }
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let data = if path.exists() {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing config {}", path.display()))?
        } else {
            ConfigData {
                fabric_label: default_fabric_label(),
                next_node_id: default_next_node_id(),
                ..ConfigData::default()
            }
        };
        Ok(Self {
            data: std::sync::Mutex::new(data),
            path: Some(path),
        })
    }

    fn save_locked(&self, data: &ConfigData) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(data)?)
            .with_context(|| format!("writing config {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("installing config {}", path.display()))?;
        Ok(())
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut ConfigData) -> T) -> Result<T> {
        let mut data = self.data.lock().unwrap();
        let out = f(&mut data);
        self.save_locked(&data)?;
        Ok(out)
    }

    // -- fabric label ------------------------------------------------------

    pub fn fabric_label(&self) -> String {
        self.data.lock().unwrap().fabric_label.clone()
    }

    /// matter.js requires a non-empty 1..=32 character label, so a null or
    /// blank request falls back to the default rather than clearing it.
    pub fn normalize_fabric_label(label: Option<&str>) -> String {
        let trimmed = label.map(str::trim).unwrap_or("");
        let effective = if trimmed.is_empty() {
            DEFAULT_FABRIC_LABEL
        } else {
            trimmed
        };
        effective.chars().take(MAX_FABRIC_LABEL_LEN).collect()
    }

    pub fn set_fabric_label(&self, label: &str) -> Result<String> {
        let normalized = Self::normalize_fabric_label(Some(label));
        self.mutate(|data| data.fabric_label = normalized.clone())?;
        Ok(normalized)
    }

    // -- node id allocation ------------------------------------------------

    /// Hand out the next operational node id, persisting the counter before it
    /// is used so a crash mid-commission can never reissue the same id.
    pub fn allocate_node_id(&self) -> Result<u64> {
        self.mutate(|data| {
            let id = data.next_node_id.max(1);
            data.next_node_id = id + 1;
            id
        })
    }

    /// Ensure the counter sits above every id already in use, which matters
    /// after restoring a node snapshot written by an older build.
    pub fn reserve_node_ids_above(&self, highest_in_use: u64) -> Result<()> {
        self.mutate(|data| {
            if data.next_node_id <= highest_in_use {
                data.next_node_id = highest_in_use + 1;
            }
        })
    }

    // -- wifi credentials --------------------------------------------------

    /// A password is required, and may be omitted only to keep the stored
    /// secret for an unchanged SSID.
    pub fn set_wifi_credentials(
        &self,
        id: Option<&str>,
        ssid: &str,
        credentials: Option<&str>,
    ) -> Result<(), String> {
        let canonical = validate_credential_id(id.unwrap_or(DEFAULT_CREDENTIAL_ID))?;
        let mut data = self.data.lock().unwrap();

        if canonical == DEFAULT_CREDENTIAL_ID {
            let reusable = if data.wifi_ssid.as_deref() == Some(ssid) {
                data.wifi_credentials.clone()
            } else {
                None
            };
            let secret = require_or_keep_secret(credentials, reusable.as_deref())?;
            data.wifi_ssid = Some(ssid.to_string());
            data.wifi_credentials = Some(secret);
        } else {
            assert_no_case_clash(
                &canonical,
                data.additional_wifi.iter().map(|e| e.id.as_str()),
            )?;
            let existing = data
                .additional_wifi
                .iter()
                .position(|entry| same_id(&entry.id, &canonical));
            let reusable = existing
                .filter(|&index| data.additional_wifi[index].ssid == ssid)
                .map(|index| data.additional_wifi[index].credentials.clone());
            let secret = require_or_keep_secret(credentials, reusable.as_deref())?;
            let entry = WifiEntry {
                id: canonical,
                ssid: ssid.to_string(),
                credentials: secret,
            };
            match existing {
                Some(index) => data.additional_wifi[index] = entry,
                None => data.additional_wifi.push(entry),
            }
        }
        self.save_locked(&data).map_err(|e| e.to_string())
    }

    pub fn remove_wifi_credentials(&self, id: Option<&str>) -> Result<()> {
        let id = id.unwrap_or(DEFAULT_CREDENTIAL_ID);
        self.mutate(|data| {
            if same_id(id, DEFAULT_CREDENTIAL_ID) {
                data.wifi_ssid = None;
                data.wifi_credentials = None;
            } else {
                data.additional_wifi.retain(|entry| !same_id(&entry.id, id));
            }
        })
    }

    /// The credentials to send when commissioning, by entry id.
    pub fn wifi_credentials(&self, id: Option<&str>) -> Option<(String, String)> {
        let id = id.unwrap_or(DEFAULT_CREDENTIAL_ID);
        let data = self.data.lock().unwrap();
        if same_id(id, DEFAULT_CREDENTIAL_ID) {
            return match (&data.wifi_ssid, &data.wifi_credentials) {
                (Some(ssid), Some(secret)) => Some((ssid.clone(), secret.clone())),
                _ => None,
            };
        }
        data.additional_wifi
            .iter()
            .find(|entry| same_id(&entry.id, id))
            .map(|entry| (entry.ssid.clone(), entry.credentials.clone()))
    }

    /// True when the reserved `default` entry holds usable credentials. This is
    /// what `server_info.wifi_credentials_set` reports.
    pub fn wifi_credentials_set(&self) -> bool {
        let data = self.data.lock().unwrap();
        data.wifi_ssid.is_some() && data.wifi_credentials.is_some()
    }

    pub fn default_wifi_ssid(&self) -> Option<String> {
        let data = self.data.lock().unwrap();
        match (&data.wifi_ssid, &data.wifi_credentials) {
            (Some(ssid), Some(_)) => Some(ssid.clone()),
            _ => None,
        }
    }

    // -- thread credentials ------------------------------------------------

    pub fn set_thread_dataset(&self, id: Option<&str>, dataset: &str) -> Result<(), String> {
        let canonical = validate_credential_id(id.unwrap_or(DEFAULT_CREDENTIAL_ID))?;
        let mut data = self.data.lock().unwrap();
        if canonical == DEFAULT_CREDENTIAL_ID {
            data.thread_dataset = Some(dataset.to_string());
        } else {
            assert_no_case_clash(
                &canonical,
                data.additional_thread.iter().map(|e| e.id.as_str()),
            )?;
            let entry = ThreadEntry {
                id: canonical,
                dataset: dataset.to_string(),
            };
            match data
                .additional_thread
                .iter()
                .position(|e| same_id(&e.id, &entry.id))
            {
                Some(index) => data.additional_thread[index] = entry,
                None => data.additional_thread.push(entry),
            }
        }
        self.save_locked(&data).map_err(|e| e.to_string())
    }

    pub fn remove_thread_dataset(&self, id: Option<&str>) -> Result<()> {
        let id = id.unwrap_or(DEFAULT_CREDENTIAL_ID);
        self.mutate(|data| {
            if same_id(id, DEFAULT_CREDENTIAL_ID) {
                data.thread_dataset = None;
            } else {
                data.additional_thread
                    .retain(|entry| !same_id(&entry.id, id));
            }
        })
    }

    pub fn thread_dataset(&self, id: Option<&str>) -> Option<String> {
        let id = id.unwrap_or(DEFAULT_CREDENTIAL_ID);
        let data = self.data.lock().unwrap();
        if same_id(id, DEFAULT_CREDENTIAL_ID) {
            return data.thread_dataset.clone();
        }
        data.additional_thread
            .iter()
            .find(|entry| same_id(&entry.id, id))
            .map(|entry| entry.dataset.clone())
    }

    pub fn thread_credentials_set(&self) -> bool {
        self.data.lock().unwrap().thread_dataset.is_some()
    }

    /// Every stored dataset, for the Thread diagnostics registry.
    pub fn all_thread_datasets(&self) -> BTreeMap<String, String> {
        let data = self.data.lock().unwrap();
        let mut all = BTreeMap::new();
        if let Some(dataset) = &data.thread_dataset {
            all.insert(DEFAULT_CREDENTIAL_ID.to_string(), dataset.clone());
        }
        for entry in &data.additional_thread {
            all.insert(entry.id.clone(), entry.dataset.clone());
        }
        all
    }

    // -- summaries ---------------------------------------------------------

    /// The `get_all_credentials` view. The reserved `default` entry is always
    /// listed even when unset, so clients can address it without a prior write.
    pub fn summaries(&self) -> AllCredentialsSummary {
        let data = self.data.lock().unwrap();

        let mut wifi = Vec::new();
        if let (Some(ssid), Some(_)) = (&data.wifi_ssid, &data.wifi_credentials) {
            wifi.push(WifiCredentialSummary {
                id: DEFAULT_CREDENTIAL_ID.to_string(),
                ssid: ssid.clone(),
            });
        }
        for entry in &data.additional_wifi {
            wifi.push(WifiCredentialSummary {
                id: entry.id.clone(),
                ssid: entry.ssid.clone(),
            });
        }
        if !wifi.iter().any(|e| e.id == DEFAULT_CREDENTIAL_ID) {
            wifi.insert(
                0,
                WifiCredentialSummary {
                    id: DEFAULT_CREDENTIAL_ID.to_string(),
                    ssid: String::new(),
                },
            );
        }

        let summarize_thread = |id: &str, dataset: &str| {
            let decoded = super::thread_dataset::decode(dataset);
            ThreadCredentialSummary {
                id: id.to_string(),
                network_name: decoded.as_ref().and_then(|d| d.network_name.clone()),
                ext_pan_id: decoded.as_ref().and_then(|d| d.ext_pan_id.clone()),
            }
        };

        let mut thread = Vec::new();
        if let Some(dataset) = &data.thread_dataset {
            thread.push(summarize_thread(DEFAULT_CREDENTIAL_ID, dataset));
        }
        for entry in &data.additional_thread {
            thread.push(summarize_thread(&entry.id, &entry.dataset));
        }
        if !thread.iter().any(|e| e.id == DEFAULT_CREDENTIAL_ID) {
            thread.insert(
                0,
                ThreadCredentialSummary {
                    id: DEFAULT_CREDENTIAL_ID.to_string(),
                    network_name: None,
                    ext_pan_id: None,
                },
            );
        }

        AllCredentialsSummary { wifi, thread }
    }
}

fn require_or_keep_secret(
    provided: Option<&str>,
    reusable: Option<&str>,
) -> Result<String, String> {
    if let Some(secret) = provided.filter(|s| !s.is_empty()) {
        return Ok(secret.to_string());
    }
    if let Some(secret) = reusable.filter(|s| !s.is_empty()) {
        return Ok(secret.to_string());
    }
    Err("WiFi password is required (omit it only to keep the existing password for an unchanged SSID)".into())
}

fn assert_no_case_clash<'a>(
    id: &str,
    existing: impl Iterator<Item = &'a str>,
) -> Result<(), String> {
    let trimmed = id.trim();
    for other in existing {
        if other.eq_ignore_ascii_case(trimmed) && other != trimmed {
            return Err(format!(
                "invalid-credential-id: '{}' duplicates existing '{}'",
                trimmed, other
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_password_may_be_omitted_only_for_an_unchanged_ssid() {
        let store = ConfigStore::in_memory();
        store
            .set_wifi_credentials(None, "home", Some("secret"))
            .unwrap();

        // Same SSID, no password: keeps the stored secret.
        store.set_wifi_credentials(None, "home", None).unwrap();
        assert_eq!(
            store.wifi_credentials(None),
            Some(("home".into(), "secret".into()))
        );

        // Different SSID, no password: rejected.
        let error = store.set_wifi_credentials(None, "guest", None).unwrap_err();
        assert!(error.contains("WiFi password is required"));
    }

    #[test]
    fn reserved_ids_are_rejected_for_named_entries() {
        let store = ConfigStore::in_memory();
        assert!(store
            .set_wifi_credentials(Some("delete"), "x", Some("y"))
            .unwrap_err()
            .contains("reserved"));
        assert!(store
            .set_wifi_credentials(Some("  "), "x", Some("y"))
            .unwrap_err()
            .contains("non-empty"));
    }

    #[test]
    fn ids_differing_only_in_case_clash() {
        let store = ConfigStore::in_memory();
        store
            .set_wifi_credentials(Some("Guest"), "g", Some("p"))
            .unwrap();
        let error = store
            .set_wifi_credentials(Some("guest"), "g2", Some("p2"))
            .unwrap_err();
        assert!(error.contains("duplicates existing"));
    }

    #[test]
    fn summaries_never_expose_secrets_and_always_list_default() {
        let store = ConfigStore::in_memory();
        store
            .set_wifi_credentials(Some("Guest"), "guest-net", Some("hunter2"))
            .unwrap();
        let summary = store.summaries();
        let rendered = serde_json::to_string(&summary).unwrap();
        assert!(rendered.contains("guest-net"));
        assert!(!rendered.contains("hunter2"));
        assert_eq!(summary.wifi[0].id, DEFAULT_CREDENTIAL_ID);
        assert_eq!(summary.wifi[0].ssid, "");
        assert_eq!(summary.thread[0].id, DEFAULT_CREDENTIAL_ID);
    }

    #[test]
    fn removing_the_default_entry_zeroes_it_but_keeps_it_listed() {
        let store = ConfigStore::in_memory();
        store
            .set_wifi_credentials(None, "home", Some("secret"))
            .unwrap();
        assert!(store.wifi_credentials_set());
        store.remove_wifi_credentials(None).unwrap();
        assert!(!store.wifi_credentials_set());
        assert_eq!(store.summaries().wifi.len(), 1);
        assert_eq!(store.summaries().wifi[0].ssid, "");
    }

    #[test]
    fn node_ids_are_allocated_sequentially_from_one() {
        let store = ConfigStore::in_memory();
        assert_eq!(store.allocate_node_id().unwrap(), 1);
        assert_eq!(store.allocate_node_id().unwrap(), 2);
        store.reserve_node_ids_above(10).unwrap();
        assert_eq!(store.allocate_node_id().unwrap(), 11);
    }

    #[test]
    fn fabric_label_falls_back_and_truncates() {
        assert_eq!(
            ConfigStore::normalize_fabric_label(None),
            DEFAULT_FABRIC_LABEL
        );
        assert_eq!(
            ConfigStore::normalize_fabric_label(Some("   ")),
            DEFAULT_FABRIC_LABEL
        );
        assert_eq!(ConfigStore::normalize_fabric_label(Some(" Home ")), "Home");
        assert_eq!(
            ConfigStore::normalize_fabric_label(Some(&"x".repeat(64))).len(),
            MAX_FABRIC_LABEL_LEN
        );
    }

    #[test]
    fn config_survives_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        {
            let store = ConfigStore::load(&path).unwrap();
            store
                .set_wifi_credentials(None, "home", Some("secret"))
                .unwrap();
            store.set_fabric_label("Living Room").unwrap();
            store.allocate_node_id().unwrap();
        }
        let reloaded = ConfigStore::load(&path).unwrap();
        assert_eq!(reloaded.fabric_label(), "Living Room");
        assert!(reloaded.wifi_credentials_set());
        assert_eq!(reloaded.allocate_node_id().unwrap(), 2);
    }
}
