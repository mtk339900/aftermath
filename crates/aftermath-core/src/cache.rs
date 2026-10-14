//! Content-hash based parse cache (Feature: fast feedback / incremental analysis).
//!
//! The cache lives in `.aftermath/cache.json` inside the repository and stores
//! parsed files keyed by the SHA-256 of their content. Unchanged files are
//! never reparsed. The cache is disposable: deleting it is always safe, and it
//! is never consulted when `cache.enabled = false`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lang::ParsedFile;

const CACHE_VERSION: u32 = 1;
const CACHE_DIR: &str = ".aftermath";
const CACHE_FILE: &str = "cache.json";

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    /// path → { hash, parsed }
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CacheEntry {
    hash: String,
    parsed: ParsedFile,
}

/// In-memory view of the on-disk cache.
pub struct ParseCache {
    path: PathBuf,
    entries: BTreeMap<String, CacheEntry>,
    enabled: bool,
    dirty: bool,
}

impl ParseCache {
    /// Load the cache for a repository. A missing or malformed cache simply
    /// starts empty — analysis must never fail because of cache state.
    pub fn load(repo_root: &Path, enabled: bool) -> Self {
        let path = repo_root.join(CACHE_DIR).join(CACHE_FILE);
        let mut entries = BTreeMap::new();
        if enabled {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(parsed) = serde_json::from_str::<CacheFile>(&text) {
                    if parsed.version == CACHE_VERSION {
                        entries = parsed.entries;
                    }
                }
            }
        }
        Self {
            path,
            entries,
            enabled,
            dirty: false,
        }
    }

    /// Return a cached parse when the content hash matches.
    pub fn get(&mut self, path: &str, hash: &str) -> Option<ParsedFile> {
        if !self.enabled {
            return None;
        }
        let entry = self.entries.get(path)?;
        if entry.hash == hash {
            Some(entry.parsed.clone())
        } else {
            None
        }
    }

    pub fn put(&mut self, path: String, hash: String, parsed: ParsedFile) {
        if !self.enabled {
            return;
        }
        self.entries.insert(path, CacheEntry { hash, parsed });
        self.dirty = true;
    }

    /// Persist the cache atomically (temp file + rename). Only `.aftermath/`
    /// is written — never source files.
    pub fn save(&self) -> std::io::Result<()> {
        if !self.enabled || !self.dirty {
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)?;
        let payload = CacheFile {
            version: CACHE_VERSION,
            entries: self.entries.clone(),
        };
        let text = serde_json::to_string(&payload)
            .map_err(|e| std::io::Error::other(format!("cache serialize: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        // Windows rename fails when the destination exists.
        let _ = std::fs::remove_file(&self.path);
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Number of cached entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ParsedFile;
    use tempfile::TempDir;

    fn sample() -> ParsedFile {
        ParsedFile::empty("src/a.ts", "typescript")
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = TempDir::new().unwrap();
        let mut cache = ParseCache::load(dir.path(), true);
        assert!(cache.get("src/a.ts", "h1").is_none());
        cache.put("src/a.ts".into(), "h1".into(), sample());
        assert!(cache.get("src/a.ts", "h1").is_some());
        assert!(
            cache.get("src/a.ts", "OTHER").is_none(),
            "hash must be verified"
        );
        cache.save().unwrap();

        let mut reloaded = ParseCache::load(dir.path(), true);
        assert!(
            reloaded.get("src/a.ts", "h1").is_some(),
            "cache must survive restart"
        );
        assert!(reloaded.get("src/a.ts", "h2").is_none());
    }

    #[test]
    fn disabled_cache_never_hits() {
        let dir = TempDir::new().unwrap();
        let mut cache = ParseCache::load(dir.path(), false);
        cache.put("src/a.ts".into(), "h1".into(), sample());
        assert!(cache.get("src/a.ts", "h1").is_none());
        cache.save().unwrap();
        assert!(!dir.path().join(CACHE_DIR).join(CACHE_FILE).exists());
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(CACHE_DIR)).unwrap();
        std::fs::write(dir.path().join(CACHE_DIR).join(CACHE_FILE), "{not json").unwrap();
        let mut cache = ParseCache::load(dir.path(), true);
        assert!(
            cache.get("x", "y").is_none(),
            "must not crash on corrupt cache"
        );
    }
}
