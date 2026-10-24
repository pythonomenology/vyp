use crate::types::conflict::ConflictSet;
use crate::types::package::VypPackage;
use crate::types::requirement::Requirement;
use crate::types::version::VypVersion;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Debug;

/// Metadata for a specific version of a package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageMetadata {
    pub package: VypPackage,
    pub version: VypVersion,
    pub dependencies: Vec<Requirement>,
    /// Conflict declarations shipped with this package version.
    pub conflict_declarations: ConflictSet,
    /// Where this metadata was sourced from (for provenance).
    pub source: String,
}

/// Available versions for a package.
#[derive(Debug, Clone)]
pub struct PackageVersions {
    pub package: VypPackage,
    pub versions: Vec<VypVersion>,
}

/// A selected wheel distribution for a resolved package+version, including the
/// integrity hashes published by the index so they can be pinned in the lock
/// file (PEP 751) rather than re-derived at install time.
#[derive(Debug, Clone)]
pub struct WheelDist {
    pub filename: String,
    pub url: String,
    /// Hash algorithm name -> hex digest, e.g. `{"sha256": "abc…"}`.
    pub hashes: HashMap<String, String>,
    pub size: Option<u64>,
}

/// Controls where package metadata comes from.
///
/// The default implementation queries PyPI Simple API.
/// Plugins can add corporate registries, local caches, artifact stores,
/// or synthesize metadata for virtual packages.
///
/// Multiple providers compose via priority ordering: the first provider
/// that returns `Some` for a package wins.
pub trait MetadataProvider: Send + Sync + Debug {
    /// Human-readable name of this provider.
    fn name(&self) -> &str;

    /// Priority order (higher = consulted first).
    fn priority(&self) -> i32;

    /// Check if this provider can supply metadata for the given package.
    fn can_provide(&self, package: &VypPackage) -> bool;

    /// List all available versions for a package.
    fn available_versions(
        &self,
        package: &VypPackage,
    ) -> Result<Option<PackageVersions>, Box<dyn std::error::Error + Send + Sync>>;

    /// Get metadata for a specific package version.
    fn get_metadata(
        &self,
        package: &VypPackage,
        version: &VypVersion,
    ) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error + Send + Sync>>;

    /// The index URL this provider fetches from, if applicable.
    /// Used to record provenance in lock files.
    fn index_url(&self) -> Option<&str> {
        None
    }

    /// Hint that these packages will be needed soon.
    /// Implementations may use this to kick off concurrent fetching of
    /// version lists and metadata. The default implementation is a no-op.
    fn prefetch(&self, _packages: &[String]) {}

    /// Hint that metadata for these specific versions will be needed soon.
    /// Called by the batch prefetcher to speculatively fetch metadata for
    /// versions that are likely to be tried next by the solver.
    fn prefetch_metadata(&self, _package: &str, _versions: &[VypVersion]) {}

    /// Non-blocking check: return versions if already available in memory.
    /// Used by the solver to prefer packages whose data has arrived from
    /// a background prefetch, avoiding unnecessary blocking.
    fn try_available_versions(
        &self,
        _package: &VypPackage,
    ) -> Option<PackageVersions> {
        None
    }

    /// Return the best wheel download URL for a resolved package+version.
    /// Used to populate wheel URLs in the lockfile so installs skip the
    /// Simple API lookup.
    fn wheel_url(
        &self,
        _package: &str,
        _version: &VypVersion,
    ) -> Option<(String, String)> {
        None
    }

    /// Return the best wheel distribution (filename, URL, hashes, size) for a
    /// resolved package+version. The default falls back to [`wheel_url`] with
    /// no hashes, so existing providers keep working; index providers override
    /// it to pin integrity hashes into the lock file.
    fn wheel_dist(
        &self,
        package: &str,
        version: &VypVersion,
    ) -> Option<WheelDist> {
        self.wheel_url(package, version).map(|(filename, url)| WheelDist {
            filename,
            url,
            hashes: HashMap::new(),
            size: None,
        })
    }

    /// The source distribution to build for a resolved package+version that
    /// has no compatible wheel. Default: none.
    fn sdist_dist(&self, _package: &str, _version: &VypVersion) -> Option<WheelDist> {
        None
    }

    /// Return profiling counters (disk hits, 304s, network fetches, etc.).
    /// Only populated when `VYP_PROFILE=1`. Default is empty.
    fn profile_data(&self) -> HashMap<String, usize> {
        HashMap::new()
    }
}
