//! Recovery drafts.
//!
//! A transcript is written here the instant recognition finishes, before the clipboard and
//! before any delivery. This is the copy that survives Huck itself crashing, and it is the
//! foundation the "recent drafts" recovery feature will be built on.
//!
//! These files contain dictated speech, which is the most sensitive data the program touches.
//! They are capped, and nothing else is written alongside them.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DraftStore {
    dir: PathBuf,
    keep: usize,
}

impl DraftStore {
    /// A store rooted at the OS app-data directory.
    pub fn at_default_location() -> Option<Self> {
        crate::paths::drafts_dir().map(|dir| DraftStore { dir, keep: 20 })
    }

    pub fn new(dir: impl Into<PathBuf>, keep: usize) -> Self {
        DraftStore { dir: dir.into(), keep }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write a draft and prune old ones. Returns the path written.
    ///
    /// An empty transcript is not worth a file, but that is not an error — the caller treats a
    /// missing draft path as "nothing to recover", not as a failure.
    pub fn save(&self, text: &str) -> std::io::Result<PathBuf> {
        if text.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty transcript",
            ));
        }
        fs::create_dir_all(&self.dir)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = self.dir.join(format!("draft-{stamp}.txt"));
        fs::write(&path, text)?;
        let _ = self.prune();
        Ok(path)
    }

    /// Most recent first.
    pub fn recent(&self) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        entries.sort();
        entries.reverse();
        entries
    }

    fn prune(&self) -> std::io::Result<()> {
        for old in self.recent().into_iter().skip(self.keep) {
            let _ = fs::remove_file(old);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hvtt-drafts-{tag}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_draft_survives_on_disk_and_can_be_read_back() {
        let dir = temp_dir("roundtrip");
        let store = DraftStore::new(&dir, 20);

        let path = store.save("words I must not lose").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "words I must not lose");
        assert_eq!(store.recent().len(), 1);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_store_creates_its_directory_if_missing() {
        let dir = temp_dir("mkdir").join("nested").join("deeper");
        let store = DraftStore::new(&dir, 5);
        assert!(store.save("hello").is_ok());
        fs::remove_dir_all(dir.parent().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn old_drafts_are_pruned_to_the_cap() {
        let dir = temp_dir("prune");
        let store = DraftStore::new(&dir, 3);
        for i in 0..6 {
            store.save(&format!("draft {i}")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(store.recent().len() <= 3, "got {}", store.recent().len());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_transcript_is_not_written() {
        let dir = temp_dir("empty");
        let store = DraftStore::new(&dir, 5);
        assert!(store.save("   ").is_err());
        assert!(store.recent().is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
