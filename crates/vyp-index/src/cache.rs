use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use vyp_api::traits::metadata_provider::PackageMetadata;
use vyp_api::VypVersion;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::{debug, warn};

const VERSION_LIST_TTL_SECS: u64 = 300; // 5 minutes

const DEFAULT_MAX_SIZE_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
const INDEX_FILE_NAME: &str = "cache-index.bin";

/// Flush the in-memory index to disk after this many mutating operations.
/// Bounds data loss while avoiding a full-index rewrite on every insert
/// (the old behavior was O(n²) disk traffic across a resolve).
const FLUSH_THRESHOLD: u32 = 64;

/// Version list plus wheel info (used in cache return types).
type CachedVersionList = (Vec<VypVersion>, HashMap<String, Vec<crate::in_memory_index::WheelInfo>>);
/// Version list with HTTP validators and wheel info.
type CachedVersionListWithValidators = (Vec<VypVersion>, Option<String>, Option<String>, HashMap<String, Vec<crate::in_memory_index::WheelInfo>>);

/// Content-addressed, LRU-evicting metadata cache.
///
/// Metadata is stored in compact binary (postcard) files named by a
/// deterministic hash of `"{package}=={version}"`. An index file tracks
/// access times for LRU eviction and is written back in batches (see
/// [`FLUSH_THRESHOLD`] and [`MetadataCache::flush`]) rather than on every
/// insert. Only metadata is cached (not wheels or sdists).
#[derive(Debug)]
pub struct MetadataCache {
    cache_dir: PathBuf,
    max_size_bytes: u64,
    index: CacheIndex,
    /// Set when `index` has unsaved changes.
    dirty: bool,
    /// Mutating ops since the last flush; triggers a flush at the threshold.
    ops_since_flush: u32,
}

impl Drop for MetadataCache {
    fn drop(&mut self) {
        // Persist any batched index changes on the way out.
        self.flush();
    }
}

/// In-memory index of cached entries with access timestamps.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheIndex {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    hash: String,
    package: String,
    version: String,
    size_bytes: u64,
    last_access: u64,
}

/// Bumped when the stored file list changes meaning; older lists are refetched.
/// 3: compact positional encoding (postcard) instead of CBOR.
/// 4: per-environment views record the full list they were built from.
const VERSION_LIST_FORMAT: u32 = 4;

/// On-disk form of a version list. Version lists are by far the largest
/// cache entries (every file of every release: megabytes for packages like
/// pydantic-core) and are decoded on every warm run, so they use a compact
/// positional encoding instead of self-describing CBOR. Positional means no
/// serde `default`/`skip` attributes here: change the layout only together
/// with [`VERSION_LIST_FORMAT`].
#[derive(Serialize, Deserialize)]
struct StoredVersionList {
    format: u32,
    /// For a per-environment view: the modification time (ns) of the full
    /// list it was derived from. 0 for a full list.
    source_mtime: u128,
    timestamp: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    versions: Vec<String>,
    files: Vec<(String, Vec<StoredFile>)>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    filename: String,
    /// The URL, minus a trailing filename when `url_is_prefix`.
    url: String,
    url_is_prefix: bool,
    has_metadata: bool,
    requires_python: Option<String>,
    yanked: bool,
    hashes: Vec<(String, String)>,
    upload_time: Option<i64>,
}

impl StoredFile {
    fn from_wheel(w: crate::in_memory_index::WheelInfo) -> Self {
        let (url, url_is_prefix) = match w.url.strip_suffix(w.filename.as_str()) {
            Some(prefix) => (prefix.to_string(), true),
            None => (w.url, false),
        };
        Self {
            url,
            url_is_prefix,
            has_metadata: w.has_metadata,
            requires_python: w.requires_python,
            yanked: w.yanked,
            hashes: w.hashes.into_iter().collect(),
            upload_time: w.upload_time,
            filename: w.filename,
        }
    }

    fn into_wheel(self) -> crate::in_memory_index::WheelInfo {
        let url = if self.url_is_prefix { self.url + &self.filename } else { self.url };
        crate::in_memory_index::WheelInfo {
            filename: self.filename,
            url,
            has_metadata: self.has_metadata,
            requires_python: self.requires_python,
            yanked: self.yanked,
            hashes: self.hashes.into_iter().collect(),
            upload_time: self.upload_time,
        }
    }
}

impl StoredVersionList {
    fn read(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        let stored: Self = postcard::from_bytes(&bytes).ok()?;
        (stored.format == VERSION_LIST_FORMAT).then_some(stored)
    }

    fn write(&self, path: &Path) -> io::Result<()> {
        let bytes = postcard::to_stdvec(self).map_err(io::Error::other)?;
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    fn versions(&self) -> Vec<VypVersion> {
        self.versions.iter().filter_map(|v| v.parse().ok()).collect()
    }

    fn wheel_info(self) -> HashMap<String, Vec<crate::in_memory_index::WheelInfo>> {
        self.files
            .into_iter()
            .map(|(v, files)| (v, files.into_iter().map(StoredFile::into_wheel).collect()))
            .collect()
    }
}

/// Deserialize a CBOR blob from `path`, returning `None` on any error.
///
/// CBOR is self-describing, so it tolerates the `#[serde(skip_serializing_if)]`
/// and `#[serde(default)]` attributes used across the shared API types — a
/// positional format like postcard or bincode would not.
fn read_bin<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = std::fs::read(path).ok()?;
    ciborium::from_reader(&bytes[..]).ok()
}

/// Serialize `value` to a CBOR blob at `path`.
fn write_bin<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes).map_err(io::Error::other)?;
    std::fs::write(path, bytes)
}

impl MetadataCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self::with_max_size(cache_dir, DEFAULT_MAX_SIZE_BYTES)
    }

    pub fn with_max_size(cache_dir: PathBuf, max_size_bytes: u64) -> Self {
        let index = Self::load_index(&cache_dir);
        Self {
            cache_dir,
            max_size_bytes,
            index,
            dirty: false,
            ops_since_flush: 0,
        }
    }

    /// Look up cached metadata. Updates the in-memory access time on hit;
    /// the timestamp is persisted on the next flush.
    pub fn get(&mut self, package: &str, version: &VypVersion) -> Option<PackageMetadata> {
        let key = Self::cache_key(package, version);
        let hash = Self::hash_key(&key);

        if let Some(entry) = self.index.entries.get_mut(&key) {
            entry.last_access = Self::now();
            self.dirty = true;
            let path = self.cache_dir.join(&hash);
            if let Some(meta) = read_bin::<PackageMetadata>(&path) {
                debug!(package, %version, "cache hit");
                return Some(meta);
            }
            // Missing or corrupt entry: drop it (deferred index write).
            warn!("dropping unreadable cache entry {}", path.display());
            self.remove_entry(&key);
            self.mark_dirty();
        }
        None
    }

    /// Store metadata in the cache. Evicts old entries if over the size limit.
    pub fn insert(&mut self, package: &str, version: &VypVersion, metadata: &PackageMetadata) {
        let key = Self::cache_key(package, version);
        let hash = Self::hash_key(&key);

        let mut bytes = Vec::new();
        if let Err(e) = ciborium::into_writer(metadata, &mut bytes) {
            warn!("failed to serialize metadata for cache: {}", e);
            return;
        }

        let size = bytes.len() as u64;

        if let Err(e) = std::fs::create_dir_all(&self.cache_dir) {
            warn!("failed to create cache dir: {}", e);
            return;
        }

        let path = self.cache_dir.join(&hash);
        if let Err(e) = std::fs::write(&path, &bytes) {
            warn!("failed to write cache file: {}", e);
            return;
        }

        self.index.entries.insert(
            key,
            CacheEntry {
                hash,
                package: package.to_string(),
                version: version.to_string(),
                size_bytes: size,
                last_access: Self::now(),
            },
        );

        self.evict_if_needed();
        self.mark_dirty();
    }

    /// Total size of all cached entries in bytes.
    pub fn total_size(&self) -> u64 {
        self.index.entries.values().map(|e| e.size_bytes).sum()
    }

    /// Number of cached entries.
    pub fn entry_count(&self) -> usize {
        self.index.entries.len()
    }

    /// Retrieve a cached version list, if present and not expired.
    pub fn get_versions(&self, package: &str) -> Option<Vec<VypVersion>> {
        self.get_versions_full(package).map(|(v, _)| v)
    }

    /// Retrieve cached version list with wheel info (for warm path metadata fetching).
    pub fn get_versions_full(
        &self,
        package: &str,
    ) -> Option<CachedVersionList> {
        Self::read_versions_full(&self.versions_path(package))
    }

    /// Where a package's version list is cached. Read it with
    /// [`MetadataCache::read_versions_full`] without holding the cache lock.
    pub fn versions_file(&self, package: &str) -> PathBuf {
        self.versions_path(package)
    }

    /// Where the per-environment view of a package's version list lives.
    /// `env_key` identifies the environment's wheel tags.
    pub fn env_versions_file(&self, package: &str, env_key: &str) -> PathBuf {
        self.cache_dir.join(format!("versions-{}.{}.pc", package, env_key))
    }

    /// A fresh cached version list reduced to the files one environment can
    /// use: wheels `compatible` accepts, plus sdists. A version left with no
    /// file is dropped (no distribution of it could be installed here).
    /// The view is stored at `view` and reused until the full list at `full`
    /// changes, so warm runs decode a few KiB instead of every file of every
    /// release for every platform.
    pub fn read_versions_for_env(
        full: &Path,
        view: &Path,
        compatible: impl Fn(&str) -> bool,
    ) -> Option<CachedVersionList> {
        let full_mtime = std::fs::metadata(full)
            .and_then(|m| m.modified())
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let now = Self::now();
        if let Some(stored) = StoredVersionList::read(view).filter(|v| v.source_mtime == full_mtime) {
            if now.saturating_sub(stored.timestamp) > VERSION_LIST_TTL_SECS {
                return None;
            }
            let versions = stored.versions();
            return Some((versions, stored.wheel_info()));
        }

        let mut stored = StoredVersionList::read(full)?;
        if now.saturating_sub(stored.timestamp) > VERSION_LIST_TTL_SECS {
            return None;
        }
        let mut emptied = std::collections::HashSet::new();
        for (version, files) in &mut stored.files {
            files.retain(|f| crate::version_filter::is_sdist(&f.filename) || compatible(&f.filename));
            if files.is_empty() {
                emptied.insert(version.clone());
            }
        }
        stored.files.retain(|(_, files)| !files.is_empty());
        stored.versions.retain(|v| !emptied.contains(v));
        stored.source_mtime = full_mtime;
        let _ = stored.write(view);
        let versions = stored.versions();
        Some((versions, stored.wheel_info()))
    }

    /// A fresh (unexpired) cached version list from `path`.
    pub fn read_versions_full(path: &Path) -> Option<CachedVersionList> {
        let stored = StoredVersionList::read(path)?;
        let now = Self::now();
        if now.saturating_sub(stored.timestamp) > VERSION_LIST_TTL_SECS {
            return None;
        }
        let versions = stored.versions();
        Some((versions, stored.wheel_info()))
    }

    /// Record an access to a cached entry and return the file holding it.
    /// Read it with [`MetadataCache::read_entry`] after releasing the lock,
    /// so concurrent lookups don't serialize on file I/O.
    pub fn entry_path(&mut self, package: &str, version: &VypVersion) -> Option<PathBuf> {
        let key = Self::cache_key(package, version);
        let entry = self.index.entries.get_mut(&key)?;
        entry.last_access = Self::now();
        self.dirty = true;
        Some(self.cache_dir.join(&entry.hash))
    }

    pub fn read_entry(path: &Path) -> Option<PackageMetadata> {
        read_bin(path)
    }

    /// Drop an entry whose file turned out to be missing or unreadable.
    pub fn forget(&mut self, package: &str, version: &VypVersion) {
        let key = Self::cache_key(package, version);
        warn!("dropping unreadable cache entry for {}", key);
        self.remove_entry(&key);
        self.mark_dirty();
    }

    /// Retrieve a stale version list together with its HTTP validators
    /// and wheel info for conditional revalidation. Returns data even when
    /// TTL-expired, but only if the file exists.
    pub fn get_versions_with_validators(
        &self,
        package: &str,
    ) -> Option<CachedVersionListWithValidators> {
        let stored = StoredVersionList::read(&self.versions_path(package))?;
        let versions = stored.versions();
        let (etag, last_modified) = (stored.etag.clone(), stored.last_modified.clone());
        Some((versions, etag, last_modified, stored.wheel_info()))
    }

    /// Refresh the timestamp on a stale version list (e.g. after a 304).
    pub fn refresh_versions_timestamp(&self, package: &str) {
        let path = self.versions_path(package);
        let Some(mut stored) = StoredVersionList::read(&path) else { return };
        stored.timestamp = Self::now();
        let _ = stored.write(&path);
    }

    /// Cache a version list with HTTP validators, wheel info, and a fresh TTL.
    pub fn insert_versions(
        &self,
        package: &str,
        versions: &[VypVersion],
        etag: Option<String>,
        last_modified: Option<String>,
        wheel_info: HashMap<String, Vec<crate::in_memory_index::WheelInfo>>,
    ) {
        let stored = StoredVersionList {
            format: VERSION_LIST_FORMAT,
            source_mtime: 0,
            timestamp: Self::now(),
            etag,
            last_modified,
            versions: versions.iter().map(|v| v.to_string()).collect(),
            files: wheel_info
                .into_iter()
                .map(|(v, files)| (v, files.into_iter().map(StoredFile::from_wheel).collect()))
                .collect(),
        };
        if std::fs::create_dir_all(&self.cache_dir).is_err() {
            return;
        }
        let _ = stored.write(&self.versions_path(package));

        self.evict_stale_version_lists();
    }

    /// Persist the in-memory index to disk if it has unsaved changes.
    pub fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        let path = self.cache_dir.join(INDEX_FILE_NAME);
        match write_bin(&path, &self.index) {
            Ok(()) => {
                self.dirty = false;
                self.ops_since_flush = 0;
            }
            Err(e) => warn!("failed to save cache index: {}", e),
        }
    }

    /// Mark the index dirty and flush opportunistically once enough mutating
    /// operations have accumulated.
    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.ops_since_flush += 1;
        if self.ops_since_flush >= FLUSH_THRESHOLD {
            self.flush();
        }
    }

    fn versions_path(&self, package: &str) -> PathBuf {
        self.cache_dir.join(format!("versions-{}.pc", package))
    }

    fn evict_stale_version_lists(&self) {
        let Ok(entries) = std::fs::read_dir(&self.cache_dir) else { return };
        let now_secs = Self::now();
        let max_age_secs = 7 * 24 * 3600;

        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if !fname.starts_with("versions-") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let modified_secs = meta.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now_secs.saturating_sub(modified_secs) > max_age_secs {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    fn evict_if_needed(&mut self) {
        let total = self.total_size();
        if total <= self.max_size_bytes {
            return;
        }

        let mut entries: Vec<_> = self.index.entries.iter()
            .map(|(k, e)| (k.clone(), e.last_access, e.size_bytes))
            .collect();
        entries.sort_by_key(|(_, access, _)| *access);

        let mut current_size = total;
        for (key, _, size) in &entries {
            if current_size <= self.max_size_bytes {
                break;
            }
            debug!(key = %key, "evicting LRU cache entry");
            self.remove_entry(key);
            current_size = current_size.saturating_sub(*size);
        }
    }

    fn remove_entry(&mut self, key: &str) {
        if let Some(entry) = self.index.entries.remove(key) {
            let path = self.cache_dir.join(&entry.hash);
            let _ = std::fs::remove_file(path);
        }
    }

    fn cache_key(package: &str, version: &VypVersion) -> String {
        format!("{}=={}", package.to_lowercase().replace(['-', '.'], "_"), version)
    }

    /// Simple deterministic hash for content-addressed filenames.
    /// Uses FNV-1a which is deterministic across runs.
    fn hash_key(key: &str) -> String {
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in key.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        format!("{:016x}.bin", hash)
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn load_index(cache_dir: &Path) -> CacheIndex {
        let path = cache_dir.join(INDEX_FILE_NAME);
        let mut index: CacheIndex = read_bin(&path).unwrap_or_default();

        Self::reconcile_orphans(cache_dir, &mut index);
        index
    }

    /// Reconcile metadata files on disk with the in-memory index.
    ///
    /// The index is persisted in batches, so a valid metadata file can exist
    /// without a matching index entry (a missed flush, a crash, or a process
    /// exiting before a background write was recorded). Because [`get`] looks an
    /// entry up in the index before reading its file, such a file would never be
    /// served — and the old behavior *deleted* it, causing a permanent cache
    /// miss and re-fetch. Instead, **adopt** untracked metadata files back into
    /// the index; only remove files that are unreadable or misnamed.
    ///
    /// [`get`]: MetadataCache::get
    fn reconcile_orphans(cache_dir: &Path, index: &mut CacheIndex) {
        let tracked_hashes: std::collections::HashSet<String> = index.entries.values()
            .map(|e| e.hash.clone())
            .collect();

        let Ok(entries) = std::fs::read_dir(cache_dir) else { return };
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname == INDEX_FILE_NAME || fname.starts_with("versions-") {
                continue;
            }
            if tracked_hashes.contains(&fname) {
                continue;
            }

            let path = entry.path();
            let Some(meta) = read_bin::<PackageMetadata>(&path) else {
                // Unreadable or a stale/foreign format: safe to remove.
                let _ = std::fs::remove_file(&path);
                continue;
            };
            let key = Self::cache_key(meta.package.name(), &meta.version);
            if Self::hash_key(&key) != fname {
                // Filename doesn't address this content; not a usable entry.
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            index.entries.entry(key).or_insert(CacheEntry {
                hash: fname,
                package: meta.package.name().to_string(),
                version: meta.version.to_string(),
                size_bytes: size,
                last_access: Self::now(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vyp_api::{VypPackage, ConflictSet};
    use std::path::PathBuf;

    fn temp_cache_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "vyp-cache-test-{}-{}",
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn sample_metadata(name: &str, version: &VypVersion) -> PackageMetadata {
        PackageMetadata {
            package: VypPackage::named(name),
            version: version.clone(),
            dependencies: Vec::new(),
            conflict_declarations: ConflictSet::new(),
            source: "test".to_string(),
        }
    }

    #[test]
    fn test_insert_and_get() {
        let dir = temp_cache_dir();
        let mut cache = MetadataCache::new(dir.clone());
        let v = VypVersion::from_parts(1, 0, 0);
        let meta = sample_metadata("test-pkg", &v);

        assert!(cache.get("test-pkg", &v).is_none());
        cache.insert("test-pkg", &v, &meta);
        assert!(cache.get("test-pkg", &v).is_some());
        assert_eq!(cache.entry_count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_orphan_metadata_file_is_adopted_not_deleted() {
        // Simulates a missed index flush (process exited before the batched
        // index write recorded a metadata file). Reopening must adopt the
        // orphan file back into the index — not delete it — so the entry stays
        // a cache hit instead of forcing a re-fetch.
        let dir = temp_cache_dir();
        let v = VypVersion::from_parts(3, 1, 0);
        let meta = sample_metadata("orphan-pkg", &v);

        {
            let mut cache = MetadataCache::new(dir.clone());
            cache.insert("orphan-pkg", &v, &meta);
            cache.flush();
        }
        // Delete only the index, leaving the metadata blob orphaned on disk.
        let _ = std::fs::remove_file(dir.join(INDEX_FILE_NAME));

        let mut recovered = MetadataCache::new(dir.clone());
        assert_eq!(recovered.entry_count(), 1, "orphan file should be adopted");
        assert!(
            recovered.get("orphan-pkg", &v).is_some(),
            "adopted entry must be retrievable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lru_eviction() {
        let dir = temp_cache_dir();
        // Very small max size to trigger eviction
        let mut cache = MetadataCache::with_max_size(dir.clone(), 200);

        for i in 0..20 {
            let v = VypVersion::from_parts(1, 0, i);
            let meta = sample_metadata(&format!("pkg-{}", i), &v);
            cache.insert(&format!("pkg-{}", i), &v, &meta);
        }

        // Should have evicted most entries to stay under 200 bytes
        assert!(cache.total_size() <= 200);
        assert!(cache.entry_count() < 20);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_persistence() {
        let dir = temp_cache_dir();
        let v = VypVersion::from_parts(2, 0, 0);
        let meta = sample_metadata("numpy", &v);

        {
            let mut cache = MetadataCache::new(dir.clone());
            cache.insert("numpy", &v, &meta);
            // Index is written back on drop (batched flush).
        }

        // New cache instance should find the entry via persisted index
        let mut cache2 = MetadataCache::new(dir.clone());
        assert!(cache2.get("numpy", &v).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_batched_flush_persists_on_drop() {
        let dir = temp_cache_dir();
        {
            let mut cache = MetadataCache::new(dir.clone());
            // Fewer than FLUSH_THRESHOLD inserts: nothing forced to disk yet,
            // but Drop must still persist the index.
            for i in 0..5 {
                let v = VypVersion::from_parts(1, 0, i);
                cache.insert(&format!("pkg-{}", i), &v, &sample_metadata("p", &v));
            }
        }
        let cache2 = MetadataCache::new(dir.clone());
        assert_eq!(cache2.entry_count(), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_version_list_roundtrip() {
        let dir = temp_cache_dir();
        let cache = MetadataCache::new(dir.clone());
        let versions = vec![VypVersion::from_parts(1, 0, 0), VypVersion::from_parts(2, 0, 0)];
        cache.insert_versions("numpy", &versions, Some("etag123".into()), None, HashMap::new());

        let (got, _) = cache.get_versions_full("numpy").unwrap();
        assert_eq!(got, versions);
        let (_, etag, _, _) = cache.get_versions_with_validators("numpy").unwrap();
        assert_eq!(etag.as_deref(), Some("etag123"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn wheel(name: &str) -> crate::in_memory_index::WheelInfo {
        crate::in_memory_index::WheelInfo {
            filename: name.to_string(),
            url: format!("https://files.example/{}", name),
            has_metadata: true,
            requires_python: None,
            yanked: false,
            hashes: [("sha256".to_string(), "ab".to_string())].into(),
            upload_time: Some(1),
        }
    }

    #[test]
    fn version_list_round_trips_compactly() {
        let dir = temp_cache_dir();
        let cache = MetadataCache::new(dir.clone());
        let versions = vec![VypVersion::from_parts(1, 0, 0)];
        let info = HashMap::from([("1.0.0".to_string(), vec![wheel("p-1.0.0-py3-none-any.whl")])]);
        cache.insert_versions("p", &versions, Some("e".into()), None, info);
        let (v, w, etag) = {
            let (v, etag, _, w) = cache.get_versions_with_validators("p").unwrap();
            (v, w, etag)
        };
        assert_eq!(v, versions);
        assert_eq!(etag.as_deref(), Some("e"));
        assert_eq!(w["1.0.0"][0].url, "https://files.example/p-1.0.0-py3-none-any.whl");
        assert_eq!(w["1.0.0"][0].hashes["sha256"], "ab");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn env_view_keeps_usable_files_and_follows_the_full_list() {
        let dir = temp_cache_dir();
        let cache = MetadataCache::new(dir.clone());
        let v = |s: &str| s.parse::<VypVersion>().unwrap();
        let info = HashMap::from([
            ("1.0".to_string(), vec![wheel("p-1.0-cp313-cp313-macosx_11_0_arm64.whl"), wheel("p-1.0-cp313-cp313-win_amd64.whl")]),
            ("2.0".to_string(), vec![wheel("p-2.0-cp313-cp313-win_amd64.whl")]),
            ("3.0".to_string(), vec![wheel("p-3.0-cp313-cp313-win_amd64.whl"), wheel("p-3.0.tar.gz")]),
        ]);
        // 4.0 has no file information at all (e.g. from an HTML index).
        cache.insert_versions("p", &[v("1.0"), v("2.0"), v("3.0"), v("4.0")], None, None, info);
        let full = cache.versions_file("p");
        let view = cache.env_versions_file("p", "test");
        let mac = |f: &str| f.contains("macosx");

        let (versions, files) = MetadataCache::read_versions_for_env(&full, &view, mac).unwrap();
        assert_eq!(versions, vec![v("1.0"), v("3.0"), v("4.0")], "2.0 has nothing usable here");
        assert_eq!(files["1.0"].len(), 1);
        assert_eq!(files["3.0"][0].filename, "p-3.0.tar.gz");
        assert!(view.exists());

        // A refreshed full list replaces the view.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let info = HashMap::from([("5.0".to_string(), vec![wheel("p-5.0-cp313-cp313-macosx_11_0_arm64.whl")])]);
        cache.insert_versions("p", &[v("5.0")], None, None, info);
        let (versions, _) = MetadataCache::read_versions_for_env(&full, &view, mac).unwrap();
        assert_eq!(versions, vec![v("5.0")]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
