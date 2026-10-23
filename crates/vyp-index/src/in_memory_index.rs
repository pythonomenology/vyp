use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use vyp_api::{VypVersion, Requirement};
use vyp_api::traits::metadata_provider::PackageMetadata;

/// Thread-safe in-memory index that bridges the solver thread and the
/// background fetcher. The solver calls `wait_blocking` to get data, and
/// the fetcher calls `set_*` to populate results.
///
/// Slots are heap-allocated behind `Arc` so that references remain valid
/// even when the DashMap rehashes and relocates its internal storage.
pub struct InMemoryIndex {
    versions: DashMap<String, Arc<Slot<VersionsResult>>>,
    metadata: DashMap<(String, VypVersion), Arc<Slot<MetadataResult>>>,
}

/// Result of fetching versions, including PEP 658 file info.
#[derive(Clone, Debug, Default)]
pub struct VersionsResult {
    pub versions: Vec<VypVersion>,
    /// Wheel files per version with PEP 658 metadata availability.
    pub wheel_info: HashMap<VypVersion, Vec<WheelInfo>>,
    /// Set when the version list could not be fetched due to a transport/HTTP
    /// error (as opposed to the package genuinely having no versions). Lets the
    /// resolver distinguish "network failure" from "no such package".
    pub fetch_error: Option<String>,
}

impl VersionsResult {
    /// An empty result carrying a network/transport error reason.
    pub fn error(reason: impl Into<String>) -> Self {
        Self {
            versions: Vec::new(),
            wheel_info: HashMap::new(),
            fetch_error: Some(reason.into()),
        }
    }
}

/// Information about a wheel file from the Simple API response.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WheelInfo {
    pub filename: String,
    pub url: String,
    pub has_metadata: bool,
    pub requires_python: Option<String>,
    /// PEP 592 yank status: `true` if the file is yanked.
    #[serde(default)]
    pub yanked: bool,
    /// Integrity hashes published by the index (algo -> hex digest).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub hashes: std::collections::BTreeMap<String, String>,
}

/// Metadata result stored in the index.
#[derive(Clone, Debug)]
pub struct MetadataResult {
    pub dependencies: Vec<Requirement>,
    pub full_metadata: Option<PackageMetadata>,
    /// Set when the fetch task ended without producing a result (e.g. it
    /// panicked), so the resolver can report it instead of silently treating
    /// the version as having no dependencies.
    pub fetch_error: Option<String>,
}

impl MetadataResult {
    /// An empty result carrying a failure reason.
    pub fn error(reason: impl Into<String>) -> Self {
        Self {
            dependencies: Vec::new(),
            full_metadata: None,
            fetch_error: Some(reason.into()),
        }
    }
}

/// A slot that starts empty and is filled exactly once.
/// Uses `Condvar` for blocking waits — lighter than `futures::executor::block_on`
/// since it avoids creating a mini async runtime on each wait.
struct Slot<T> {
    value: std::sync::Mutex<Option<Arc<T>>>,
    ready: std::sync::Condvar,
}

impl<T> Slot<T> {
    fn new() -> Self {
        Self {
            value: std::sync::Mutex::new(None),
            ready: std::sync::Condvar::new(),
        }
    }

    fn set(&self, val: T) {
        *self.value.lock().expect("poisoned") = Some(Arc::new(val));
        self.ready.notify_all();
    }

    /// Store `val()` only if the slot is still empty. Runs from `Drop` during
    /// unwinding, so it tolerates a poisoned lock rather than panicking again.
    fn set_if_empty(&self, val: impl FnOnce() -> T) {
        let mut guard = self.value.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(Arc::new(val()));
            drop(guard);
            self.ready.notify_all();
        }
    }

    fn wait_blocking(&self) -> Arc<T> {
        let mut guard = self.value.lock().expect("poisoned");
        loop {
            if let Some(v) = guard.as_ref() {
                return Arc::clone(v);
            }
            guard = self.ready.wait(guard).expect("poisoned");
        }
    }

    fn try_get(&self) -> Option<Arc<T>> {
        self.value.lock().expect("poisoned").as_ref().map(Arc::clone)
    }
}

impl InMemoryIndex {
    pub fn new() -> Self {
        Self {
            versions: DashMap::new(),
            metadata: DashMap::new(),
        }
    }

    pub fn register_versions(&self, package: &str) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.versions.entry(package.to_string()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(Arc::new(Slot::new()));
                true
            }
        }
    }

    pub fn wait_versions(&self, package: &str) -> Arc<VersionsResult> {
        let slot = self.versions
            .entry(package.to_string())
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone();
        slot.wait_blocking()
    }

    pub fn try_get_versions(&self, package: &str) -> Option<Arc<VersionsResult>> {
        let entry = self.versions.get(package)?;
        entry.value().try_get()
    }

    pub fn get_wheel_info(&self, package: &str, version: &VypVersion) -> Option<Vec<WheelInfo>> {
        let entry = self.versions.get(package)?;
        entry.value().try_get().and_then(|r| r.wheel_info.get(version).cloned())
    }

    pub fn set_versions(&self, package: &str, result: VersionsResult) {
        let slot = self.versions
            .entry(package.to_string())
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone();
        slot.set(result);
    }

    pub fn register_metadata(&self, package: &str, version: &VypVersion) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.metadata.entry((package.to_string(), version.clone())) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(Arc::new(Slot::new()));
                true
            }
        }
    }

    pub fn wait_metadata(&self, package: &str, version: &VypVersion) -> Arc<MetadataResult> {
        let key = (package.to_string(), version.clone());
        let slot = self.metadata
            .entry(key)
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone();
        slot.wait_blocking()
    }

    pub fn try_get_metadata(
        &self,
        package: &str,
        version: &VypVersion,
    ) -> Option<Arc<MetadataResult>> {
        let key = (package.to_string(), version.clone());
        let entry = self.metadata.get(&key)?;
        entry.value().try_get()
    }

    pub fn set_metadata(
        &self,
        package: &str,
        version: &VypVersion,
        result: MetadataResult,
    ) {
        let key = (package.to_string(), version.clone());
        let slot = self.metadata
            .entry(key)
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone();
        slot.set(result);
    }
}

/// Held by the task that won `register_versions`. If the task ends without
/// calling `set_versions` (a panic, which tokio swallows, or an early return),
/// dropping the claim fills the slot with an error so `wait_versions` returns
/// instead of blocking the solver forever.
#[must_use = "dropping the claim immediately fills the slot with an error"]
pub struct VersionsClaim {
    index: Arc<InMemoryIndex>,
    package: String,
}

impl VersionsClaim {
    pub fn new(index: Arc<InMemoryIndex>, package: &str) -> Self {
        Self { index, package: package.to_string() }
    }
}

impl Drop for VersionsClaim {
    fn drop(&mut self) {
        let slot = self.index.versions
            .entry(self.package.clone())
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone();
        slot.set_if_empty(|| VersionsResult::error(format!(
            "version fetch for {} ended without a result", self.package
        )));
    }
}

/// Held by the task that won `register_metadata`; the metadata counterpart of
/// [`VersionsClaim`].
#[must_use = "dropping the claim immediately fills the slot with an error"]
pub struct MetadataClaim {
    index: Arc<InMemoryIndex>,
    package: String,
    version: VypVersion,
}

impl MetadataClaim {
    pub fn new(index: Arc<InMemoryIndex>, package: &str, version: &VypVersion) -> Self {
        Self { index, package: package.to_string(), version: version.clone() }
    }
}

impl Drop for MetadataClaim {
    fn drop(&mut self) {
        let slot = self.index.metadata
            .entry((self.package.clone(), self.version.clone()))
            .or_insert_with(|| Arc::new(Slot::new()))
            .clone();
        slot.set_if_empty(|| MetadataResult::error(format!(
            "metadata fetch for {}=={} ended without a result", self.package, self.version
        )));
    }
}

impl Default for InMemoryIndex {
    fn default() -> Self {
        Self::new()
    }
}
