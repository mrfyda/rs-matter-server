//! Persistent controller state that is not owned by rs-matter.
//!
//! rs-matter persists the fabric and its certificates; everything else the
//! protocol promises to survive a restart — nodes, interview results,
//! credentials, the node-id counter, the fabric label — lives here.

pub mod config;
pub mod nodes;
pub mod thread_dataset;

pub use config::ConfigStore;
pub use nodes::{InterviewDiff, NodeStore, StoredNode};

/// Writing state that must not be world-readable.
///
/// The storage directory holds the fabric's signing material and the Wi-Fi
/// password in cleartext, so everything in it is private to the user the
/// server runs as. Two mechanisms rather than one: `restrict_new_files` covers
/// files rs-matter creates on its own (the key-value blobs, the ICAC key),
/// which this crate never names, and `write_private` covers ours.
pub mod private {
    use std::fs;
    use std::io;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    /// Owner-only, for both the files and the directory holding them.
    const FILE_MODE: u32 = 0o600;
    const DIR_MODE: u32 = 0o700;

    /// Make every file this process creates from now on owner-only.
    ///
    /// A umask is the only way to reach files written by a dependency, and it
    /// is inherited by nothing here — the server spawns no children.
    pub fn restrict_new_files() {
        // SAFETY: `umask` cannot fail and touches only this process. It is
        // called once, before any thread that could create a file starts.
        unsafe { libc::umask(0o077) };
    }

    /// Write a file that only the owner can read, through a temporary file so
    /// an interrupted write cannot truncate what was already there.
    pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
        let tmp = path.with_extension("tmp");

        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(FILE_MODE)
                .open(&tmp)?;
            io::Write::write_all(&mut file, contents)?;
            file.sync_all()?;
        }

        // `create` leaves an existing file's mode alone, so set it explicitly
        // rather than trusting the file to be new.
        fs::set_permissions(&tmp, fs::Permissions::from_mode(FILE_MODE))?;
        fs::rename(&tmp, path)
    }

    /// Tighten anything already on disk from an earlier version that wrote
    /// with the default mode. Best-effort: a file that cannot be chmodded is
    /// reported and skipped rather than stopping startup.
    pub fn tighten_existing(dir: &Path) {
        if let Err(error) = fs::set_permissions(dir, fs::Permissions::from_mode(DIR_MODE)) {
            log::warn!("Could not restrict {}: {}", dir.display(), error);
        }

        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                log::warn!("Could not list {}: {}", dir.display(), error);
                return;
            }
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let mode = entry.metadata().map(|m| m.permissions().mode() & 0o777);
            if mode.map(|m| m == FILE_MODE).unwrap_or(false) {
                continue;
            }
            if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(FILE_MODE)) {
                log::warn!("Could not restrict {}: {}", path.display(), error);
            } else {
                log::info!("Restricted {} to owner-only", path.display());
            }
        }
    }
}

#[cfg(test)]
mod private_tests {
    use super::private::{tighten_existing, write_private};
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn written_state_is_not_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        write_private(&path, b"{}").unwrap();

        assert_eq!(
            mode_of(&path),
            0o600,
            "the fabric key and Wi-Fi password live here"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }

    #[test]
    fn rewriting_keeps_the_mode_of_a_file_that_already_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"new").unwrap();

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn an_upgrade_tightens_what_an_earlier_version_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let loose = dir.path().join("controller-icac-key.bin");
        std::fs::write(&loose, b"key").unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();

        tighten_existing(dir.path());

        assert_eq!(mode_of(&loose), 0o600);
        assert_eq!(mode_of(dir.path()), 0o700);
    }
}
