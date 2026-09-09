//! Reading a matterjs-server storage directory.
//!
//! matter.js keeps one *namespace* per subdirectory of its storage path
//! (`config` for the server's own settings, `server` for the controller), and
//! each namespace is a flat map of dotted context paths to key/value pairs:
//!
//! ```text
//! credentials          → fabric, rootCertBytes, rootKeyPair, …
//! nodes                → commissionedNodes
//! nodes.peer1.endpoints.0.commissioning → peerAddress, commissionedAt, …
//! ```
//!
//! Three drivers write that same logical model in three different shapes, and
//! which one is in use is recorded in `driver.json`. All three are read here
//! because all three are in the field: `wal` is what matterjs-server has
//! configured today, `file` is what older installs have, and `json` is a
//! supported option. This module normalises them into one map so nothing
//! downstream has to care which was used.
//!
//! Everything here is read-only, deliberately: the source directory stays
//! exactly as it was so a user who wants to go back to matterjs-server can.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::value::MjValue;

/// The storage layout a namespace was written with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// A gzipped snapshot plus a write-ahead log of JSON-line commits.
    Wal,
    /// One file per key, named after the dotted context path.
    File,
    /// A single `storage.json` holding the whole namespace.
    Json,
}

impl Driver {
    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "wal" => Some(Self::Wal),
            "file" => Some(Self::File),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Contexts and their keys, exactly as matter.js sees them.
#[derive(Debug)]
pub struct Namespace {
    pub name: String,
    pub driver: Driver,
    contexts: BTreeMap<String, BTreeMap<String, MjValue>>,
}

impl Namespace {
    /// Look one key up, addressed the way matter.js addresses it.
    pub fn get(&self, contexts: &[&str], key: &str) -> Option<&MjValue> {
        self.contexts.get(&contexts.join("."))?.get(key)
    }

    /// Every key in one context.
    pub fn context(&self, contexts: &[&str]) -> Option<&BTreeMap<String, MjValue>> {
        self.contexts.get(&contexts.join("."))
    }

    /// The immediate child context names under a path — how the per-node
    /// contexts (`nodes.peer1`, `nodes.peer2`, …) are discovered, since
    /// nothing records how many there are.
    pub fn child_contexts(&self, contexts: &[&str]) -> Vec<String> {
        let prefix = if contexts.is_empty() {
            String::new()
        } else {
            format!("{}.", contexts.join("."))
        };

        let mut children: Vec<String> = self
            .contexts
            .keys()
            .filter_map(|path| {
                let rest = path.strip_prefix(&prefix)?;
                if prefix.is_empty() && path.is_empty() {
                    return None;
                }
                rest.split('.').next().map(str::to_string)
            })
            .filter(|name| !name.is_empty())
            .collect();
        children.sort();
        children.dedup();
        children
    }

    pub fn is_empty(&self) -> bool {
        self.contexts.values().all(BTreeMap::is_empty)
    }
}

/// One namespace directory, loaded or refused.
#[derive(Debug)]
enum Loaded {
    Namespace(Namespace),
    /// Present, but written by a driver this reader does not implement. Kept
    /// rather than dropped so the failure names the driver when the namespace
    /// turns out to be one the import needs.
    Unsupported(String),
}

/// A matterjs-server storage directory.
#[derive(Debug)]
pub struct MatterJsStorage {
    pub root: PathBuf,
    namespaces: BTreeMap<String, Loaded>,
}

impl MatterJsStorage {
    /// Read every namespace under `root`.
    ///
    /// Directories that are not matter.js storage (`ota-uploads`, say) are
    /// skipped silently; a namespace that is storage but unreadable is an
    /// error, because the caller cannot tell whether it held the fabric.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            bail!(
                "'{}' is not a directory; point --import-matterjs at matterjs-server's \
                 --storage-path (the directory holding 'config' and 'server')",
                root.display()
            );
        }

        let mut namespaces = BTreeMap::new();
        for entry in std::fs::read_dir(&root)
            .with_context(|| format!("listing {}", root.display()))?
            .flatten()
        {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();

            let Some(driver) = detect_driver(&path)? else {
                continue;
            };
            match driver {
                Some(driver) => {
                    let contexts = match driver {
                        Driver::Wal => read_wal(&path),
                        Driver::File => read_files(&path),
                        Driver::Json => read_json(&path),
                    }
                    .with_context(|| {
                        format!("reading the '{}' namespace at {}", name, path.display())
                    })?;
                    namespaces.insert(
                        name.clone(),
                        Loaded::Namespace(Namespace {
                            name,
                            driver,
                            contexts,
                        }),
                    );
                }
                None => {
                    let kind = read_descriptor_kind(&path)?.unwrap_or_else(|| "unknown".into());
                    namespaces.insert(name, Loaded::Unsupported(kind));
                }
            }
        }

        Ok(Self { root, namespaces })
    }

    /// A namespace by name, with an error that says what to do when it is
    /// missing or was written by a driver this reader cannot read.
    pub fn namespace(&self, name: &str) -> Result<&Namespace> {
        match self.namespaces.get(name) {
            Some(Loaded::Namespace(namespace)) => Ok(namespace),
            Some(Loaded::Unsupported(driver)) => bail!(
                "the '{}' namespace in {} was written by matter.js's '{}' storage driver, \
                 which this importer cannot read. Start matterjs-server once with \
                 MATTER_STORAGE_DRIVER=wal to convert it, then import again",
                name,
                self.root.display(),
                driver
            ),
            None => bail!(
                "no '{}' namespace in {}. Point --import-matterjs at matterjs-server's \
                 --storage-path (the directory holding 'config' and 'server')",
                name,
                self.root.display()
            ),
        }
    }

    pub fn has_namespace(&self, name: &str) -> bool {
        matches!(self.namespaces.get(name), Some(Loaded::Namespace(_)))
    }

    /// Namespace names, for diagnostics and for finding the controller's own
    /// namespace when it is not the default `server`.
    pub fn namespace_names(&self) -> Vec<String> {
        self.namespaces.keys().cloned().collect()
    }
}

/// Which driver wrote a directory, if it is storage at all.
///
/// `Ok(None)` means "not a storage directory"; `Ok(Some(None))` means storage
/// written by a driver this reader does not implement. The detection mirrors
/// matter.js's own: `driver.json` first, then the legacy fallbacks it applies
/// when that file predates the directory.
fn detect_driver(path: &Path) -> Result<Option<Option<Driver>>> {
    if let Some(kind) = read_descriptor_kind(path)? {
        return Ok(Some(Driver::parse(&kind)));
    }

    if path.join("wal").is_dir()
        || path.join("snapshot.json.gz").is_file()
        || path.join("snapshot.json").is_file()
    {
        return Ok(Some(Some(Driver::Wal)));
    }
    if path.join("storage.json").is_file() {
        return Ok(Some(Some(Driver::Json)));
    }

    // matter.js's own rule: a directory holding any non-reserved file, with no
    // descriptor, is the legacy one-file-per-key format.
    let has_data = std::fs::read_dir(path)
        .with_context(|| format!("listing {}", path.display()))?
        .flatten()
        .any(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
                && !is_reserved(&entry.file_name().to_string_lossy())
        });

    Ok(has_data.then_some(Some(Driver::File)))
}

fn read_descriptor_kind(path: &Path) -> Result<Option<String>> {
    let descriptor = path.join("driver.json");
    if !descriptor.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&descriptor)
        .with_context(|| format!("reading {}", descriptor.display()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", descriptor.display()))?;

    if parsed.get("type").and_then(serde_json::Value::as_str) == Some("blob") {
        // Blob namespaces hold uploaded firmware images, not controller state.
        return Ok(Some("blob".into()));
    }
    Ok(parsed
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string))
}

fn is_reserved(name: &str) -> bool {
    matches!(name, "driver.json" | "matter.lock" | "matter.pid")
}

type Contexts = BTreeMap<String, BTreeMap<String, MjValue>>;

/// A WAL position: the segment file and the line within it.
type CommitId = (u64, u64);

/// A snapshot and the commit it was taken at. Commits at or before that point
/// are already in the data and must not be replayed on top of it.
struct Snapshot {
    contexts: Contexts,
    commit_id: Option<CommitId>,
}

/// The `file` driver: one file per key, the filename being the URI-encoded
/// dotted path with the key as its last segment.
fn read_files(path: &Path) -> Result<Contexts> {
    let mut contexts = Contexts::new();

    for entry in std::fs::read_dir(path)?.flatten() {
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let raw_name = entry.file_name().to_string_lossy().into_owned();
        if is_reserved(&raw_name) || raw_name.ends_with(".tmp") {
            continue;
        }

        let decoded = percent_decode(&raw_name)?;
        let mut parts: Vec<&str> = decoded.split('.').collect();
        let Some(key) = parts.pop() else { continue };
        let context = parts.join(".");

        let text = std::fs::read_to_string(entry.path())
            .with_context(|| format!("reading {}", entry.path().display()))?;
        // matter.js skips a value it cannot parse rather than refusing to
        // start; a file this import does not read must not stop it either.
        let Ok(value) = MjValue::from_json_str(&text) else {
            log::warn!(
                "Skipping unparseable storage file {}",
                entry.path().display()
            );
            continue;
        };

        contexts
            .entry(context)
            .or_default()
            .insert(key.to_string(), value);
    }

    Ok(contexts)
}

/// The `json` driver: the whole namespace in one file, in the same shape the
/// WAL snapshot uses.
fn read_json(path: &Path) -> Result<Contexts> {
    let file = path.join("storage.json");
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let raw: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
    store_data(&raw)
}

/// The `wal` driver: a snapshot plus every commit written after it.
fn read_wal(path: &Path) -> Result<Contexts> {
    let mut contexts = Contexts::new();
    let mut after: Option<CommitId> = None;

    if let Some(snapshot) = read_snapshot(path)? {
        contexts = snapshot.contexts;
        after = snapshot.commit_id;
    }

    let wal_dir = path.join("wal");
    if wal_dir.is_dir() {
        for (segment, file) in wal_segments(&wal_dir)? {
            // Line numbers count blank lines too: they are what matter.js's
            // own reader counts when it decides which commits a snapshot
            // already covers, and an off-by-one here would replay a commit
            // twice or skip one.
            for (offset, line) in read_lines(&file)?.into_iter().enumerate() {
                if let Some((snap_segment, snap_offset)) = after {
                    if (segment, offset as u64) <= (snap_segment, snap_offset) {
                        continue;
                    }
                }
                if line.trim().is_empty() {
                    continue;
                }
                apply_commit(&mut contexts, &line).with_context(|| {
                    format!(
                        "applying commit {}:{} from {}",
                        segment,
                        offset,
                        file.display()
                    )
                })?;
            }
        }
    }

    Ok(contexts)
}

fn read_snapshot(path: &Path) -> Result<Option<Snapshot>> {
    let gzipped = path.join("snapshot.json.gz");
    let plain = path.join("snapshot.json");

    // Both can exist after a driver switch; matter.js takes the newer.
    let source = match (gzipped.is_file(), plain.is_file()) {
        (true, true) => {
            let newer = |a: &Path, b: &Path| -> bool {
                let stamp = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
                match (stamp(a), stamp(b)) {
                    (Some(a), Some(b)) => a >= b,
                    _ => true,
                }
            };
            if newer(&gzipped, &plain) {
                Some(gzipped)
            } else {
                Some(plain)
            }
        }
        (true, false) => Some(gzipped),
        (false, true) => Some(plain),
        (false, false) => None,
    };

    let Some(source) = source else {
        return Ok(None);
    };

    let text = read_maybe_gzipped(&source)?;
    let raw: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing the snapshot {}", source.display()))?;

    let commit_id = raw.get("commitId").and_then(|id| {
        Some((
            id.get("segment")?.as_u64()?,
            id.get("offset")?.as_u64().unwrap_or(0),
        ))
    });
    let data = raw
        .get("data")
        .ok_or_else(|| anyhow!("the snapshot {} has no data", source.display()))?;

    Ok(Some(Snapshot {
        contexts: store_data(data)?,
        commit_id,
    }))
}

/// Segment files in replay order, preferring the compressed copy of a segment
/// when both exist — matter.js compresses in place and deletes afterwards, so
/// a crash can leave both behind.
fn wal_segments(dir: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut segments: BTreeMap<u64, PathBuf> = BTreeMap::new();

    for entry in std::fs::read_dir(dir)?.flatten() {
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let (digits, compressed) = match name.strip_suffix(".jsonl.gz") {
            Some(digits) => (digits, true),
            None => match name.strip_suffix(".jsonl") {
                Some(digits) => (digits, false),
                None => continue,
            },
        };
        if digits.len() != 8 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let segment = u64::from_str_radix(digits, 16)?;

        let slot = segments.entry(segment).or_insert_with(|| entry.path());
        if compressed {
            *slot = entry.path();
        }
    }

    Ok(segments.into_iter().collect())
}

fn apply_commit(contexts: &mut Contexts, line: &str) -> Result<()> {
    let commit: serde_json::Value = match serde_json::from_str(line) {
        Ok(commit) => commit,
        // matter.js skips a torn line rather than refusing to start, and so
        // must we: the last line of a WAL is exactly what a hard kill truncates.
        Err(error) => {
            log::warn!("Skipping a malformed matter.js WAL line: {}", error);
            return Ok(());
        }
    };

    // A commit is `{ts, ops}`; very old ones are a bare array of ops.
    let ops = match commit.get("ops") {
        Some(ops) => ops,
        None => &commit,
    };
    let Some(ops) = ops.as_array() else {
        return Ok(());
    };

    for op in ops {
        let kind = op.get("op").and_then(serde_json::Value::as_str);
        let key = op
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        match kind {
            Some("upd") => {
                let Some(values) = op.get("values").and_then(serde_json::Value::as_object) else {
                    continue;
                };
                let context = contexts.entry(key).or_default();
                for (name, value) in values {
                    context.insert(name.clone(), MjValue::from_json(value.clone()));
                }
            }
            Some("del") => match op.get("values").and_then(serde_json::Value::as_array) {
                // Named keys within one context.
                Some(names) => {
                    if let Some(context) = contexts.get_mut(&key) {
                        for name in names.iter().filter_map(serde_json::Value::as_str) {
                            context.remove(name);
                        }
                    }
                }
                // A whole context and everything beneath it — or, with an
                // empty key, the entire namespace.
                None => {
                    if key.is_empty() {
                        contexts.clear();
                    } else {
                        let prefix = format!("{}.", key);
                        contexts.remove(&key);
                        contexts.retain(|path, _| !path.starts_with(&prefix));
                    }
                }
            },
            _ => {}
        }
    }

    Ok(())
}

/// `{context: {key: value}}` — the shape both the snapshot and the `json`
/// driver store.
fn store_data(raw: &serde_json::Value) -> Result<Contexts> {
    let mut contexts = Contexts::new();
    let Some(fields) = raw.as_object() else {
        return Ok(contexts);
    };

    for (context, values) in fields {
        let Some(values) = values.as_object() else {
            continue;
        };
        let entry = contexts.entry(context.clone()).or_default();
        for (key, value) in values {
            entry.insert(key.clone(), MjValue::from_json(value.clone()));
        }
    }

    Ok(contexts)
}

fn read_maybe_gzipped(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

    // Sniff rather than trust the extension: a `.json` written by an older
    // build and a `.json.gz` renamed by hand both turn up in the field.
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut text = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut text)
            .with_context(|| format!("decompressing {}", path.display()))?;
        return Ok(text);
    }

    String::from_utf8(bytes).with_context(|| format!("{} is not UTF-8", path.display()))
}

fn read_lines(path: &Path) -> Result<Vec<String>> {
    Ok(read_maybe_gzipped(path)?
        .split('\n')
        .map(str::to_string)
        .collect())
}

/// `decodeURIComponent`, for the `file` driver's filenames.
fn percent_decode(name: &str) -> Result<String> {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])?;
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }

    String::from_utf8(out).with_context(|| format!("decoding the storage filename '{}'", name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn gzip(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(contents.as_bytes()).unwrap();
        std::fs::write(path, encoder.finish().unwrap()).unwrap();
    }

    #[test]
    fn the_wal_replays_commits_written_after_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        write(&ns.join("driver.json"), r#"{"kind":"wal","type":"kv"}"#);
        gzip(
            &ns.join("snapshot.json.gz"),
            r#"{"commitId":{"segment":0,"offset":2},"ts":1,"data":{"credentials":{"label":"old"}}}"#,
        );
        write(
            &ns.join("wal/00000000.jsonl"),
            // Offsets 0..2 are covered by the snapshot and must not be replayed.
            &[
                r#"{"ts":1,"ops":[{"op":"upd","key":"credentials","values":{"label":"stale"}}]}"#,
                r#"{"ts":1,"ops":[{"op":"upd","key":"credentials","values":{"label":"stale"}}]}"#,
                r#"{"ts":1,"ops":[{"op":"upd","key":"credentials","values":{"label":"stale"}}]}"#,
                r#"{"ts":2,"ops":[{"op":"upd","key":"credentials","values":{"label":"new"}}]}"#,
                "",
            ]
            .join("\n"),
        );

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        let namespace = storage.namespace("server").unwrap();
        assert_eq!(namespace.driver, Driver::Wal);
        assert_eq!(
            namespace.get(&["credentials"], "label").unwrap().as_str(),
            Some("new")
        );
    }

    #[test]
    fn a_deleted_context_takes_its_children_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        write(&ns.join("driver.json"), r#"{"kind":"wal"}"#);
        write(
            &ns.join("wal/00000000.jsonl"),
            &[
                r#"{"ts":1,"ops":[{"op":"upd","key":"nodes.peer1","values":{"a":1}}]}"#,
                r#"{"ts":1,"ops":[{"op":"upd","key":"nodes.peer1.endpoints","values":{"b":2}}]}"#,
                r#"{"ts":2,"ops":[{"op":"del","key":"nodes.peer1"}]}"#,
            ]
            .join("\n"),
        );

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        let namespace = storage.namespace("server").unwrap();
        assert!(namespace.get(&["nodes", "peer1"], "a").is_none());
        assert!(
            namespace
                .get(&["nodes", "peer1", "endpoints"], "b")
                .is_none(),
            "deleting a context must delete everything beneath it"
        );
    }

    #[test]
    fn a_gzipped_segment_wins_over_the_uncompressed_copy() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        write(&ns.join("driver.json"), r#"{"kind":"wal"}"#);
        write(
            &ns.join("wal/00000001.jsonl"),
            r#"{"ts":1,"ops":[{"op":"upd","key":"c","values":{"k":"uncompressed"}}]}"#,
        );
        gzip(
            &ns.join("wal/00000001.jsonl.gz"),
            r#"{"ts":1,"ops":[{"op":"upd","key":"c","values":{"k":"compressed"}}]}"#,
        );

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        assert_eq!(
            storage
                .namespace("server")
                .unwrap()
                .get(&["c"], "k")
                .unwrap()
                .as_str(),
            Some("compressed")
        );
    }

    #[test]
    fn the_legacy_file_driver_is_read_without_a_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        // "0.MatterController.fabric" style: dotted context, key last.
        write(&ns.join("credentials.fabric"), r#"{"label":"Home"}"#);
        write(&ns.join("nodes.commissionedNodes"), "[]");

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        let namespace = storage.namespace("server").unwrap();
        assert_eq!(namespace.driver, Driver::File);
        assert_eq!(
            namespace
                .get(&["credentials"], "fabric")
                .and_then(|value| value.get("label"))
                .and_then(MjValue::as_str),
            Some("Home")
        );
    }

    #[test]
    fn json_namespaces_and_child_contexts_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        write(&ns.join("driver.json"), r#"{"kind":"json"}"#);
        write(
            &ns.join("storage.json"),
            r#"{
                "nodes":{"commissionedNodes":[]},
                "nodes.peer1.endpoints.0.commissioning":{"commissionedAt":17},
                "nodes.peer2.endpoints.0.commissioning":{"commissionedAt":18}
            }"#,
        );

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        let namespace = storage.namespace("server").unwrap();
        assert_eq!(namespace.child_contexts(&["nodes"]), vec!["peer1", "peer2"]);
        assert_eq!(
            namespace
                .get(
                    &["nodes", "peer1", "endpoints", "0", "commissioning"],
                    "commissionedAt"
                )
                .and_then(MjValue::as_u64),
            Some(17)
        );
    }

    #[test]
    fn one_unreadable_value_does_not_take_the_namespace_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        write(&ns.join("driver.json"), r#"{"kind":"json"}"#);
        write(
            &ns.join("storage.json"),
            r#"{
                "nodes.peer1.endpoints.1.6": {
                    "0": "{\"__object__\":\"SomethingNew\",\"__value__\":\"1\"}"
                },
                "credentials": { "label": "Home" }
            }"#,
        );

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        let namespace = storage.namespace("server").unwrap();
        assert_eq!(
            namespace.get(&["credentials"], "label").unwrap().as_str(),
            Some("Home"),
            "a value type from a newer matter.js, in state no import reads, \
             must not block the values that matter"
        );
    }

    #[test]
    fn an_unsupported_driver_is_reported_against_the_namespace_that_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let ns = dir.path().join("server");
        write(&ns.join("driver.json"), r#"{"kind":"sqlite","type":"kv"}"#);

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        let error = storage.namespace("server").unwrap_err().to_string();
        assert!(error.contains("sqlite"), "{}", error);
        assert!(error.contains("MATTER_STORAGE_DRIVER=wal"), "{}", error);
    }

    #[test]
    fn a_missing_namespace_says_where_to_point_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ota-uploads")).unwrap();

        let storage = MatterJsStorage::open(dir.path()).unwrap();
        assert!(storage.namespace_names().is_empty());
        let error = storage.namespace("server").unwrap_err().to_string();
        assert!(error.contains("--storage-path"), "{}", error);
    }

    #[test]
    fn filenames_are_uri_decoded() {
        assert_eq!(
            percent_decode("node-1.0.29.attributeList").unwrap(),
            "node-1.0.29.attributeList"
        );
        assert_eq!(percent_decode("a%2Fb.key").unwrap(), "a/b.key");
        assert_eq!(percent_decode("100%").unwrap(), "100%");
    }
}
