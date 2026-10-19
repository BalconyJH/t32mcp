// SPDX-FileCopyrightText: 2026 Lauterbach GmbH
// SPDX-License-Identifier: Apache-2.0

//! Opt-in local evidence. Artifact IDs never accept user-controlled paths.

use rand::RngExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIRECTORY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 128;

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

pub fn validate_id(id: &str) -> Result<(), String> {
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("artifact ID must contain exactly 32 hexadecimal characters".to_string());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl ArtifactStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Called before target access when persistence is requested. Never evicts
    /// old evidence; concurrent server processes should use separate roots.
    pub fn prepare(&self) -> Result<(), String> {
        if !self.root.is_absolute() {
            return Err("artifact directory must be an absolute local path".into());
        }
        reject_network_path(&self.root)?;
        reject_link_components(&self.root, true)?;
        fs::create_dir_all(&self.root)
            .map_err(|e| format!("Cannot create artifact directory: {e}"))?;
        let root = self.canonical_root()?;
        self.check_capacity(&root, MAX_ARTIFACT_BYTES as u64)
    }

    fn canonical_root(&self) -> Result<PathBuf, String> {
        if !self.root.is_absolute() {
            return Err("artifact directory must be an absolute local path".into());
        }
        reject_network_path(&self.root)?;
        reject_link_components(&self.root, false)?;
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("Cannot resolve artifact directory: {e}"))?;
        reject_network_path(&root)?;
        if !root.is_dir() {
            return Err("artifact root is not a directory".into());
        }
        Ok(root)
    }

    fn check_capacity(&self, root: &Path, additional_bytes: u64) -> Result<(), String> {
        let mut bytes = 0u64;
        let entries =
            fs::read_dir(root).map_err(|e| format!("Cannot inspect artifact directory: {e}"))?;
        for (index, entry) in entries.enumerate() {
            if index >= MAX_DIRECTORY_ENTRIES - 1 {
                return Err(
                    "artifact directory entry limit reached; archive evidence manually or choose another --artifacts directory".into(),
                );
            }
            let entry = entry.map_err(|e| format!("Cannot inspect artifact entry: {e}"))?;
            let metadata = fs::symlink_metadata(entry.path())
                .map_err(|e| format!("Cannot inspect artifact metadata: {e}"))?;
            bytes = bytes.saturating_add(metadata.len());
            if bytes.saturating_add(additional_bytes) > MAX_DIRECTORY_BYTES {
                return Err(
                    "artifact directory size limit reached; old evidence is never automatically removed".into(),
                );
            }
        }
        Ok(())
    }

    pub fn save(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        if !matches!(kind, "snapshot" | "image_verification") {
            return Err("unsupported artifact kind".into());
        }
        let root = self.canonical_root()?;
        let id = format!("{:032x}", rand::rng().random::<u128>());
        let envelope = json!({
            "format": "t32mcp.artifact",
            "version": 1,
            "id": id,
            "kind": kind,
            "created_unix_ms": now_ms(),
            "server_version": env!("CARGO_PKG_VERSION"),
            "payload": payload,
        });
        let bytes =
            serde_json::to_vec(&envelope).map_err(|e| format!("Cannot encode evidence: {e}"))?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes"));
        }
        self.check_capacity(&root, bytes.len() as u64)?;
        let destination = root.join(format!("{id}.json"));
        let mut temporary = tempfile::NamedTempFile::new_in(&root)
            .map_err(|e| format!("Cannot create temporary evidence file: {e}"))?;
        temporary
            .write_all(&bytes)
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|e| format!("Cannot write evidence: {e}"))?;
        temporary
            .persist_noclobber(&destination)
            .map_err(|e| format!("Cannot retain evidence without overwriting: {e}"))?;
        Ok(json!({
            "saved": true,
            "id": id,
            "kind": kind,
            "path": display_path(&destination),
            "bytes": bytes.len(),
            "sha256": sha256(&bytes),
        }))
    }

    pub fn load(&self, id: &str, expected_kind: &str) -> Result<Value, String> {
        validate_id(id)?;
        let root = self.canonical_root()?;
        let path = root.join(format!("{}.json", id.to_ascii_lowercase()));
        reject_link_components(&path, false)?;
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("Cannot inspect artifact {id}: {e}"))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err("artifact must be a regular file, not a symbolic link".into());
        }
        if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
            return Err(format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes"));
        }
        let resolved = path
            .canonicalize()
            .map_err(|e| format!("Cannot resolve artifact: {e}"))?;
        if resolved.parent() != Some(root.as_path()) {
            return Err("artifact resolves outside the configured directory".into());
        }
        let file =
            fs::File::open(&resolved).map_err(|e| format!("Cannot open artifact {id}: {e}"))?;
        let mut bytes = Vec::new();
        file.take(MAX_ARTIFACT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("Cannot read artifact {id}: {e}"))?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(format!("artifact exceeds {MAX_ARTIFACT_BYTES} bytes"));
        }
        let document: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("Invalid artifact JSON: {e}"))?;
        if document["format"] != "t32mcp.artifact"
            || document["version"] != 1
            || document["id"] != id.to_ascii_lowercase()
            || document["kind"] != expected_kind
            || !document["payload"].is_object()
        {
            return Err("artifact format, ID, kind or payload does not match".into());
        }
        Ok(document["payload"].clone())
    }
}

fn reject_network_path(path: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(root_path: *const u16) -> u32;
        }
        let text = path.to_str().ok_or("artifact path must be valid Unicode")?;
        validate_windows_path(text, |drive| {
            let root: Vec<u16> = std::ffi::OsStr::new(&format!("{drive}:\\"))
                .encode_wide()
                .chain(Some(0))
                .collect();
            unsafe { GetDriveTypeW(root.as_ptr()) }
        })?;
    }
    #[cfg(not(windows))]
    let _ = path;
    Ok(())
}

#[cfg(any(windows, test))]
fn validate_windows_path(text: &str, drive_type: impl FnOnce(char) -> u32) -> Result<(), String> {
    let text = text.strip_prefix("\\\\?\\").unwrap_or(text);
    let bytes = text.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return Err("artifact path must be an absolute local drive path".into());
    }
    for component in text[3..].split(['\\', '/']) {
        if component.is_empty() {
            continue;
        }
        if component.ends_with(['.', ' '])
            || component
                .chars()
                .any(|value| value.is_control() || "<>:\"|?*".contains(value))
        {
            return Err("artifact path has an ambiguous component or alternate data stream".into());
        }
        if crate::windows_paths::is_device_name(component) {
            return Err("artifact path contains a Windows device name".into());
        }
    }
    if !matches!(drive_type(char::from(bytes[0])), 2 | 3 | 5 | 6) {
        return Err("artifact path is not on a recognized local drive".into());
    }
    Ok(())
}

fn reject_link_attributes(is_symlink: bool, attributes: u32) -> Result<(), String> {
    if is_symlink || attributes & 0x400 != 0 {
        return Err("artifact paths must not contain symbolic links or reparse points".into());
    }
    Ok(())
}

fn reject_link_components(path: &Path, allow_missing: bool) -> Result<(), String> {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        // Verbatim drive prefixes can report absolute before their RootDir
        // component, but "\\?\C:" alone is not a path that can be inspected.
        if matches!(component, std::path::Component::Prefix(_)) || !prefix.is_absolute() {
            continue;
        }
        let metadata = match fs::symlink_metadata(&prefix) {
            Ok(metadata) => metadata,
            Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(format!("Cannot inspect artifact path component: {error}")),
        };
        #[cfg(windows)]
        let attributes = {
            use std::os::windows::fs::MetadataExt;
            metadata.file_attributes()
        };
        #[cfg(not(windows))]
        let attributes = 0;
        reject_link_attributes(metadata.file_type().is_symlink(), attributes)?;
    }
    Ok(())
}

fn display_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(prefix)) = path.components().next()
            && matches!(prefix.kind(), Prefix::VerbatimDisk(_))
            && let Some(text) = path.to_str().and_then(|text| text.strip_prefix("\\\\?\\"))
        {
            return PathBuf::from(text);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_roundtrip_preserves_old_files_and_rejects_wrong_kind() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(directory.path().to_path_buf());
        store.prepare().unwrap();
        let payload = json!({"items": [{"read": {"ok": false, "error": "original failure"}}]});
        let first = store.save("snapshot", &payload).unwrap();
        let second = store.save("snapshot", &json!({"items": []})).unwrap();
        assert_ne!(first["id"], second["id"]);
        let id = first["id"].as_str().unwrap();
        assert_eq!(store.load(id, "snapshot").unwrap(), payload);
        assert!(store.load(id, "image_verification").is_err());
        let bytes = fs::read(first["path"].as_str().unwrap()).unwrap();
        assert_eq!(first["sha256"], sha256(&bytes));
        let expected_path =
            display_path(&directory.path().canonicalize().unwrap()).join(format!("{id}.json"));
        assert_eq!(Path::new(first["path"].as_str().unwrap()), expected_path);
        #[cfg(windows)]
        assert!(!first["path"].as_str().unwrap().starts_with(r"\\?\"));
    }

    #[test]
    fn rejects_paths_disguised_as_ids_before_filesystem_access() {
        let store = ArtifactStore::new(PathBuf::from("missing"));
        for id in [
            "",
            "..",
            "../outside",
            "C:\\outside",
            "a.json",
            &"g".repeat(32),
        ] {
            assert!(
                store
                    .load(id, "snapshot")
                    .unwrap_err()
                    .contains("32 hexadecimal")
            );
        }
    }

    #[test]
    fn malformed_and_oversized_artifacts_are_not_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(directory.path().to_path_buf());
        let id = "a".repeat(32);
        let path = directory.path().join(format!("{id}.json"));
        fs::write(&path, b"{broken").unwrap();
        assert!(store.load(&id, "snapshot").unwrap_err().contains("JSON"));
        fs::write(&path, b"{}").unwrap();
        assert!(store.load(&id, "snapshot").is_err());
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_ARTIFACT_BYTES as u64 + 1).unwrap();
        assert!(store.load(&id, "snapshot").unwrap_err().contains("exceeds"));
    }

    #[test]
    fn capacity_never_deletes_existing_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(directory.path().to_path_buf());
        let file = fs::File::create(directory.path().join("original-evidence")).unwrap();
        file.set_len(MAX_DIRECTORY_BYTES).unwrap();
        assert!(store.prepare().is_err());
        assert_eq!(file.metadata().unwrap().len(), MAX_DIRECTORY_BYTES);
    }

    #[test]
    fn prepare_rejects_relative_directories() {
        let store = ArtifactStore::new(PathBuf::from("relative-evidence"));
        assert!(store.prepare().unwrap_err().contains("absolute"));
    }

    #[test]
    fn rejects_remote_unknown_and_absent_drive_classifications_offline() {
        for classification in [0, 1, 4, 7] {
            assert!(validate_windows_path(r"Z:\evidence", |_| classification).is_err());
        }
        for classification in [2, 3, 5, 6] {
            assert!(validate_windows_path(r"C:\evidence", |_| classification).is_ok());
        }
    }

    #[test]
    fn rejects_unsafe_windows_paths_before_drive_query() {
        for path in [
            r"\\server\share\evidence",
            r"\\?\UNC\server\share",
            r"\\.\C:\evidence",
            r"C:evidence",
            r"C:\evidence:stream",
            r"C:\NUL.json",
            r"C:\COM1",
            r"C:\LPT9.txt",
            r"C:\name.\evidence",
            r"C:\name \evidence",
            r"C:\..\evidence",
            r"C:\.\evidence",
            r"C:\bad?\evidence",
        ] {
            assert!(
                validate_windows_path(path, |_| panic!("queried drive for unsafe path")).is_err(),
                "{path}"
            );
        }
        assert!(validate_windows_path(r"\\?\C:\evidence", |_| 3).is_ok());
    }

    #[test]
    fn rejects_fake_reparse_attributes_and_symlinks() {
        assert!(reject_link_attributes(false, 0x10).is_ok());
        assert!(reject_link_attributes(false, 0x410).is_err());
        assert!(reject_link_attributes(true, 0).is_err());
    }

    #[test]
    fn only_prepare_accepts_and_creates_missing_tail() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("missing").join("evidence");
        let store = ArtifactStore::new(root.clone());
        assert!(store.save("snapshot", &json!({})).is_err());
        assert!(store.load(&"a".repeat(32), "snapshot").is_err());
        assert!(!root.exists());
        store.prepare().unwrap();
        assert!(root.is_dir());
        assert!(store.save("snapshot", &json!({})).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn lexical_prevalidation_precedes_directory_creation() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("not-created");
        let store = ArtifactStore::new(missing.join("ambiguous."));
        assert!(store.prepare().unwrap_err().contains("ambiguous"));
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_link_is_rejected_before_creating_missing_tail() {
        let directory = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(target.path(), &link).unwrap();
        let store = ArtifactStore::new(link.join("not-created"));
        assert!(store.prepare().unwrap_err().contains("symbolic links"));
        assert!(!target.path().join("not-created").exists());
    }
}
