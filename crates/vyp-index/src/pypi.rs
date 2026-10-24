use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use vyp_api::traits::metadata_provider::{MetadataProvider, PackageMetadata, PackageVersions};
use vyp_api::{IndexScope, MarkerEnvironment, VypPackage, VypVersion, ConflictSet, Requirement};

use crate::in_memory_index::{
    InMemoryIndex, MetadataClaim, MetadataResult, VersionsClaim, VersionsResult, WheelInfo,
};
use crate::wheel_compat::PlatformTags;
use crate::wheel_metadata::fetch_wheel_metadata;

const MAX_CONCURRENT_FETCHES: usize = 500;

/// Number of retries for transient index fetch failures (network errors,
/// 429, and 5xx responses) before surfacing the error to the resolver.
const MAX_FETCH_RETRIES: u32 = 3;

/// Whether an HTTP status warrants a retry (rate-limit or server error).
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Exponential backoff with a small base, capped, for fetch retries.
async fn backoff(attempt: u32) {
    let base_ms = 100u64 << attempt.min(4); // 100, 200, 400, 800, 1600
    tokio::time::sleep(Duration::from_millis(base_ms.min(2000))).await;
}

/// Counters for profiling cache/network behaviour during resolution.
#[derive(Debug, Default)]
pub struct ProfileCounters {
    pub version_disk_hits: usize,
    pub version_304s: usize,
    pub version_fresh_fetches: usize,
    pub metadata_disk_hits: usize,
    pub metadata_network_fetches: usize,
}

/// PyPI Simple API v1+JSON metadata provider.
///
/// Uses a multi-threaded tokio runtime for all async HTTP I/O. The solver
/// thread calls the synchronous `MetadataProvider` trait methods which use
/// `runtime.block_on()` to drive async requests on the runtime's worker
/// threads. This avoids the deadlock that occurs with a `current_thread`
/// runtime where `block_on` and the I/O driver share the same thread.
pub struct PyPIMetadataProvider {
    provider_name: String,
    index_url: String,
    /// When set, restricts this index to packages the scope allows. `None`
    /// means an unscoped (primary) index that serves every package.
    scope: Option<Arc<dyn IndexScope>>,
    index: Arc<InMemoryIndex>,
    client: reqwest::Client,
    /// Async runtime driving all HTTP I/O. Shared process-wide so that
    /// multiple providers (e.g. PyPI plus scoped accelerator indexes) reuse a
    /// single worker pool instead of each spawning `num_cpus` threads.
    runtime: Arc<tokio::runtime::Runtime>,
    disk_cache: Arc<Mutex<crate::cache::MetadataCache>>,
    marker_env: MarkerEnvironment,
    /// Platform tags for `marker_env`, built once (fixed for this provider's
    /// lifetime) so version filtering and wheel selection don't rebuild them
    /// on every call on the solver hot path.
    platform_tags: PlatformTags,
    /// Target interpreter version for `Requires-Python` checks.
    target_python: Option<VypVersion>,
    /// Memoized `Requires-Python` specifier evaluations, keyed by specifier.
    rp_cache: Mutex<HashMap<String, bool>>,
    semaphore: Arc<Semaphore>,
    profile_counters: Arc<Mutex<ProfileCounters>>,
    sdists: crate::version_filter::SdistPolicy,
    exclude_newer: Arc<crate::version_filter::ExcludeNewer>,
    sdist_builder: Option<crate::sdist::SdistMetadataBuilder>,
    /// Set once a warning about missing upload times has been printed.
    warned_no_upload_time: std::sync::atomic::AtomicBool,
}

/// The process-shared multi-threaded async runtime used by every provider.
/// Follow-up fetches that share a provider's client should run here so they
/// reuse its pooled connections.
pub fn shared_runtime() -> Arc<tokio::runtime::Runtime> {
    static RUNTIME: std::sync::OnceLock<Arc<tokio::runtime::Runtime>> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let num_workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8);
            Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(num_workers)
                    .enable_all()
                    .build()
                    .expect("failed to build shared tokio runtime"),
            )
        })
        .clone()
}

impl std::fmt::Debug for PyPIMetadataProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PyPIMetadataProvider")
            .field("provider_name", &self.provider_name)
            .field("index_url", &self.index_url)
            .finish()
    }
}

impl PyPIMetadataProvider {
    pub fn new(index_url: &str) -> Self {
        Self::with_name("pypi", index_url, None)
    }

    pub fn with_name(
        name: &str,
        index_url: &str,
        scope: Option<Arc<dyn IndexScope>>,
    ) -> Self {
        Self::with_name_and_env(name, index_url, scope, None)
    }

    pub fn with_name_and_env(
        name: &str,
        index_url: &str,
        scope: Option<Arc<dyn IndexScope>>,
        marker_env: Option<MarkerEnvironment>,
    ) -> Self {
        Self::with_name_env_client(name, index_url, scope, marker_env, None)
    }

    /// Full constructor: accepts an optional external HTTP client for sharing
    /// connections between the resolver and installer.
    pub fn with_name_env_client(
        name: &str,
        index_url: &str,
        scope: Option<Arc<dyn IndexScope>>,
        marker_env: Option<MarkerEnvironment>,
        shared_client: Option<reqwest::Client>,
    ) -> Self {
        let index_url_owned = index_url.trim_end_matches('/').to_string();
        let cache_dir = Self::cache_dir_for(&index_url_owned);
        let disk_cache = Arc::new(Mutex::new(crate::cache::MetadataCache::new(cache_dir)));
        let index = Arc::new(InMemoryIndex::new());
        let marker_env = marker_env.unwrap_or_else(MarkerEnvironment::current);

        let client = shared_client.unwrap_or_else(Self::build_default_client);

        let platform_tags = PlatformTags::from_env(&marker_env);
        let target_python = marker_env.python_full_version.parse::<VypVersion>().ok();

        Self {
            provider_name: name.to_string(),
            index_url: index_url_owned,
            scope,
            index,
            client,
            runtime: shared_runtime(),
            disk_cache,
            marker_env,
            platform_tags,
            target_python,
            rp_cache: Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
            profile_counters: Arc::new(Mutex::new(ProfileCounters::default())),
            sdists: Default::default(),
            exclude_newer: Default::default(),
            sdist_builder: None,
            warned_no_upload_time: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Set the sdist policy and upload-time cutoff for this resolution.
    pub fn with_resolution_options(
        mut self,
        sdists: crate::version_filter::SdistPolicy,
        exclude_newer: Arc<crate::version_filter::ExcludeNewer>,
    ) -> Self {
        self.sdists = sdists;
        self.exclude_newer = exclude_newer;
        self
    }

    /// Set how to obtain METADATA for sdists without static metadata.
    pub fn with_sdist_builder(mut self, builder: crate::sdist::SdistMetadataBuilder) -> Self {
        self.sdist_builder = Some(builder);
        self
    }

    /// Drop versions with no installable distribution for the target
    /// environment, reusing the provider's prebuilt tags and memo cache.
    fn filter_viable(
        &self,
        package: &str,
        versions: &mut Vec<VypVersion>,
        wheel_info: &HashMap<VypVersion, Vec<WheelInfo>>,
    ) {
        let cutoff = self.exclude_newer.cutoff_for(package);
        if cutoff.is_some()
            && wheel_info.values().flatten().any(|w| w.upload_time.is_none())
            && !self.warned_no_upload_time.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!(
                "warning: {} doesn't report upload times for every file; exclude-newer can't be applied to those files",
                self.index_url
            );
        }
        let Some(target) = &self.target_python else { return };
        let mut cache = self.rp_cache.lock().expect("poisoned");
        crate::version_filter::filter_versions_with(
            versions,
            wheel_info,
            &self.platform_tags,
            target,
            &mut cache,
            &crate::version_filter::FilterOptions { sdists: self.sdists, uploaded_before: cutoff },
        );
    }

    /// Return the underlying HTTP client. `reqwest::Client` is `Arc`-based so
    /// cloning is cheap. Used to share TLS sessions between resolve and install.
    pub fn http_client(&self) -> reqwest::Client {
        self.client.clone()
    }

    pub fn build_default_client() -> reqwest::Client {
        reqwest::Client::builder()
            .user_agent(format!("vyp/{}", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .http2_initial_stream_window_size(2 * 1024 * 1024)
            .http2_initial_connection_window_size(4 * 1024 * 1024)
            .http2_keep_alive_interval(Duration::from_secs(20))
            .build()
            .expect("failed to build HTTP client")
    }

    pub fn pypi() -> Self {
        Self::new("https://pypi.org/simple")
    }

    pub fn url(&self) -> &str {
        &self.index_url
    }

    pub fn profile_counters(&self) -> Arc<Mutex<ProfileCounters>> {
        Arc::clone(&self.profile_counters)
    }

    fn default_cache_dir() -> std::path::PathBuf {
        dirs_or_default().join("vyp").join("cache").join("metadata")
    }

    /// The disk cache is keyed by package name, so each index needs its own
    /// directory: otherwise one index's cached answer for a package (say,
    /// PyPI's 404) is served for another index on the next warm run. PyPI
    /// keeps the original location so existing caches stay warm.
    fn cache_dir_for(index_url: &str) -> std::path::PathBuf {
        if index_url == "https://pypi.org/simple" {
            return Self::default_cache_dir();
        }
        let key: String = index_url
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        dirs_or_default()
            .join("vyp")
            .join("cache")
            .join("metadata-indexes")
            .join(key)
    }

    fn matches_filter(&self, package: &str) -> bool {
        match &self.scope {
            None => true,
            Some(scope) => scope.allows(package),
        }
    }

    /// Ensure a version-list fetch is in progress for this package.
    /// Does NOT block the solver thread at all — spawns an async task
    /// that checks disk cache and falls back to HTTP. The solver should
    /// call `index.wait_versions()` to block on the Notify.
    fn ensure_versions_fetching(&self, normalized: &str) {
        if self.index.try_get_versions(normalized).is_some() {
            return;
        }

        if !self.index.register_versions(normalized) {
            return;
        }

        // Everything else runs in an async task — no disk I/O on the solver thread.
        let client = self.client.clone();
        let index_url = self.index_url.clone();
        let index = Arc::clone(&self.index);
        let disk_cache = Arc::clone(&self.disk_cache);
        let semaphore = Arc::clone(&self.semaphore);
        let marker_env = self.marker_env.clone();
        let counters = Arc::clone(&self.profile_counters);
        let pkg = normalized.to_string();
        let tags = self.platform_tags.clone();
        let claim = VersionsClaim::new(Arc::clone(&self.index), normalized);

        self.runtime.spawn(async move {
            // Fills the slot with an error if this task ends without a result.
            let _claim = claim;
            // Disk cache fast path: the lock only yields the path, the read
            // happens outside it.
            let (versions_file, view_file) = {
                let cache = disk_cache.lock().expect("poisoned");
                (cache.versions_file(&pkg), cache.env_versions_file(&pkg, &tags.fingerprint()))
            };
            if let Some((versions, stored_wheel_info)) =
                crate::cache::MetadataCache::read_versions_for_env(&versions_file, &view_file, |f| {
                    f.ends_with(".whl") && tags.is_compatible(f)
                })
            {
                if let Ok(mut c) = counters.lock() { c.version_disk_hits += 1; }
                let wheel_info: HashMap<VypVersion, Vec<WheelInfo>> = stored_wheel_info.into_iter()
                    .filter_map(|(v_str, wheels)| {
                        v_str.parse::<VypVersion>().ok().map(|v| (v, wheels))
                    })
                    .collect();
                // Warm the likely picks' metadata now, in parallel with the
                // other packages, rather than one at a time as the solver asks.
                let candidates = speculative_candidates(&wheel_info);
                index.set_versions(&pkg, VersionsResult {
                    versions,
                    wheel_info,
                    fetch_error: None,
                });
                Self::prefetch_candidates(
                    &pkg, candidates, false, &client, &index, &disk_cache, &semaphore, &marker_env, &counters,
                ).await;
                return;
            }

            // Stale cache — conditional request path.
            let stale = disk_cache.lock().expect("poisoned")
                .get_versions_with_validators(&pkg);

            if let Some((versions, etag, lm, stale_wheel_info)) = stale {
                match fetch_versions_async(
                    &client, &index_url, &pkg, &semaphore,
                    etag.as_deref(), lm.as_deref(),
                ).await {
                    Ok(FetchVersionsOutcome::NotModified) => {
                        if let Ok(mut c) = counters.lock() { c.version_304s += 1; }
                        let wheel_info = stale_wheel_info.into_iter()
                            .filter_map(|(v_str, wheels)| {
                                v_str.parse::<VypVersion>().ok().map(|v| (v, wheels))
                            })
                            .collect();
                        index.set_versions(&pkg, VersionsResult {
                            versions,
                            wheel_info,
                            fetch_error: None,
                        });
                        let cache = Arc::clone(&disk_cache);
                        let p = pkg.clone();
                        tokio::task::spawn_blocking(move || {
                            cache.lock().expect("poisoned")
                                .refresh_versions_timestamp(&p);
                        });
                        return;
                    }
                    Ok(fresh @ FetchVersionsOutcome::Fresh { .. }) => {
                        if let Ok(mut c) = counters.lock() { c.version_fresh_fetches += 1; }
                        Self::handle_fresh_versions(
                            fresh, &pkg, &client, &index, &disk_cache,
                            &semaphore, &marker_env, &counters,
                        ).await;
                        return;
                    }
                    Err(e) => {
                        tracing::debug!("conditional fetch failed for {}: {}", pkg, e);
                        index.set_versions(&pkg, VersionsResult::error(format!(
                            "failed to fetch versions for {} from {}: {}",
                            pkg, index_url, e
                        )));
                        return;
                    }
                }
            }

            // No cache at all — full HTTP fetch.
            if let Ok(mut c) = counters.lock() { c.version_fresh_fetches += 1; }
            match fetch_versions_async(
                &client, &index_url, &pkg, &semaphore, None, None,
            ).await {
                Ok(fresh @ FetchVersionsOutcome::Fresh { .. }) => {
                    Self::handle_fresh_versions(
                        fresh, &pkg, &client, &index, &disk_cache,
                        &semaphore, &marker_env, &counters,
                    ).await;
                }
                Ok(FetchVersionsOutcome::NotModified) => {
                    index.set_versions(&pkg, VersionsResult {
                        versions: Vec::new(),
                        wheel_info: HashMap::new(),
                        fetch_error: None,
                    });
                }
                Err(e) => {
                    tracing::debug!("failed to fetch versions for {}: {}", pkg, e);
                    index.set_versions(&pkg, VersionsResult::error(format!(
                        "failed to fetch versions for {} from {}: {}",
                        pkg, index_url, e
                    )));
                }
            }
        });
    }

    /// Ensure a metadata fetch is in progress for this package version.
    /// Does NOT block the solver thread — spawns an async task.
    fn ensure_metadata_fetching(&self, normalized: &str, version: &VypVersion) {
        if self.index.try_get_metadata(normalized, version).is_some() {
            return;
        }

        // The wheel list decides where METADATA comes from, so it must be
        // known first. Callers that skip `available_versions` (an exact-pin
        // override, or a provider asked only for metadata) would otherwise
        // find no wheels and record the version as having no dependencies.
        self.ensure_versions_fetching(normalized);
        let versions = self.index.wait_versions(normalized);

        if !self.index.register_metadata(normalized, version) {
            return;
        }

        // Two ways to obtain METADATA: a PEP 658 sidecar (preferred, one cheap
        // GET) or, for indexes without it (e.g. download.pytorch.org), a range
        // request into any wheel of this version.
        let wheels = versions.wheel_info.get(version).cloned();
        let pep658_url = wheels.as_ref().and_then(|ws| {
            ws.iter().find(|w| w.has_metadata && w.filename.ends_with(".whl")).map(|w| w.url.clone())
        });
        let range_wheel_url = wheels.as_ref().and_then(|ws| best_wheel_url(ws, &self.marker_env));
        // Last resort: the sdist, when no wheel of this version exists.
        let sdist_file = if pep658_url.is_none() && range_wheel_url.is_none() {
            wheels.as_ref().and_then(|ws| {
                ws.iter()
                    .filter(|w| crate::version_filter::is_sdist(&w.filename) && !w.yanked)
                    .min_by_key(|w| !w.filename.ends_with(".tar.gz"))
                    .cloned()
            })
        } else {
            None
        };
        let sdist_builder = self.sdist_builder.clone();
        // With no wheel to read and a failed version list, report that failure
        // rather than an empty dependency list.
        let versions_error = versions.fetch_error.clone()
            .filter(|_| pep658_url.is_none() && range_wheel_url.is_none());

        let client = self.client.clone();
        let idx = Arc::clone(&self.index);
        let disk_cache = Arc::clone(&self.disk_cache);
        let env = self.marker_env.clone();
        let sem = Arc::clone(&self.semaphore);
        let counters = Arc::clone(&self.profile_counters);
        let pkg = normalized.to_string();
        let ver = version.clone();
        let claim = MetadataClaim::new(Arc::clone(&self.index), normalized, version);

        self.runtime.spawn(async move {
            // Fills the slot with an error if this task ends without a result.
            let _claim = claim;
            let cached = cached_metadata(&disk_cache, &pkg, &ver);

            if let Some(cached_meta) = cached {
                if let Ok(mut c) = counters.lock() { c.metadata_disk_hits += 1; }
                idx.set_metadata(&pkg, &ver, MetadataResult {
                    dependencies: cached_meta.dependencies.clone(),
                    full_metadata: Some(cached_meta),
                    fetch_error: None,
                });
                return;
            }

            if let Ok(mut c) = counters.lock() { c.metadata_network_fetches += 1; }

            // Prefer PEP 658; fall back to a ranged wheel fetch.
            let fetched: Option<PackageMetadata> = if let Some(ref url) = pep658_url {
                let metadata_url = format!("{}.metadata", url);
                fetch_metadata_async(&client, &metadata_url, &pkg, &ver, &env, &sem).await.ok()
            } else if let Some(ref wheel_url) = range_wheel_url {
                match fetch_wheel_metadata(&client, wheel_url, &sem).await {
                    Ok(text) => Some(metadata_from_text(&text, &pkg, &ver, &env, wheel_url.clone())),
                    Err(e) => {
                        tracing::debug!("range metadata fetch failed for {}=={}: {}", pkg, ver, e);
                        None
                    }
                }
            } else if let Some(sdist) = sdist_file {
                match sdist_metadata(&client, &sdist, sdist_builder, &sem).await {
                    Ok(text) => Some(metadata_from_text(&text, &pkg, &ver, &env, sdist.url.clone())),
                    Err(e) => {
                        let message = format!("cannot get metadata for {}=={} from its sdist: {}", pkg, ver, e);
                        tracing::warn!("{}", message);
                        idx.set_metadata(&pkg, &ver, MetadataResult {
                            dependencies: Vec::new(),
                            full_metadata: None,
                            fetch_error: Some(message),
                        });
                        return;
                    }
                }
            } else {
                None
            };

            match fetched {
                Some(meta) => {
                    idx.set_metadata(&pkg, &ver, MetadataResult {
                        dependencies: meta.dependencies.clone(),
                        full_metadata: Some(meta.clone()),
                        fetch_error: None,
                    });
                    let p = pkg;
                    let v = ver;
                    // Await the write so it isn't dropped when the runtime is
                    // shut down at process exit. The solver already unblocked
                    // via set_metadata above, so this does not slow resolution —
                    // it only guarantees the entry is persisted, avoiding a
                    // permanent cache miss (and re-fetch) on subsequent runs.
                    let _ = tokio::task::spawn_blocking(move || {
                        disk_cache.lock().expect("poisoned").insert(&p, &v, &meta);
                    })
                    .await;
                }
                None => {
                    idx.set_metadata(&pkg, &ver, MetadataResult {
                        dependencies: Vec::new(),
                        full_metadata: None,
                        fetch_error: versions_error,
                    });
                }
            }
        });
    }

    /// Process a fresh version response: populate index, cache to disk,
    /// and chain metadata prefetches for the most likely candidate versions.
    #[allow(clippy::too_many_arguments)]
    async fn handle_fresh_versions(
        outcome: FetchVersionsOutcome,
        pkg: &str,
        client: &reqwest::Client,
        index: &Arc<InMemoryIndex>,
        disk_cache: &Arc<Mutex<crate::cache::MetadataCache>>,
        semaphore: &Arc<Semaphore>,
        marker_env: &MarkerEnvironment,
        counters: &Arc<Mutex<ProfileCounters>>,
    ) {
        let FetchVersionsOutcome::Fresh { result, etag, last_modified } = outcome else {
            return;
        };

        let candidates = speculative_candidates(&result.wheel_info);

        let vers_for_cache = result.versions.clone();
        let wheel_info_for_cache: HashMap<String, Vec<WheelInfo>> = result.wheel_info.iter()
            .map(|(v, w)| (v.to_string(), w.clone()))
            .collect();
        index.set_versions(pkg, result);

        {
            let cache = Arc::clone(disk_cache);
            let p = pkg.to_string();
            tokio::task::spawn_blocking(move || {
                cache.lock().expect("poisoned")
                    .insert_versions(&p, &vers_for_cache, etag, last_modified, wheel_info_for_cache);
            });
        }

        Self::prefetch_candidates(
            pkg, candidates, true, client, index, disk_cache, semaphore, marker_env, counters,
        ).await;
    }

    /// Fetch metadata for the likely picks of a package: from the disk cache
    /// when present, otherwise (with `network`) via PEP 658. Warm runs pass
    /// `network = false`, so a guess never costs a request; the solver
    /// fetches on demand what it actually needs.
    #[allow(clippy::too_many_arguments)]
    async fn prefetch_candidates(
        pkg: &str,
        candidates: Vec<(VypVersion, String)>,
        network: bool,
        client: &reqwest::Client,
        index: &Arc<InMemoryIndex>,
        disk_cache: &Arc<Mutex<crate::cache::MetadataCache>>,
        semaphore: &Arc<Semaphore>,
        marker_env: &MarkerEnvironment,
        counters: &Arc<Mutex<ProfileCounters>>,
    ) {
        for (version, wheel_url) in candidates {
            if index.try_get_metadata(pkg, &version).is_some() {
                continue;
            }
            let cached_meta = cached_metadata(disk_cache, pkg, &version);
            if cached_meta.is_none() && !network {
                continue;
            }
            if !index.register_metadata(pkg, &version) {
                continue;
            }
            let claim = MetadataClaim::new(Arc::clone(index), pkg, &version);
            if let Some(cached_meta) = cached_meta {
                if let Ok(mut c) = counters.lock() { c.metadata_disk_hits += 1; }
                index.set_metadata(pkg, &version, MetadataResult {
                    dependencies: cached_meta.dependencies.clone(),
                    full_metadata: Some(cached_meta),
                    fetch_error: None,
                });
                continue;
            }
            if let Ok(mut c) = counters.lock() { c.metadata_network_fetches += 1; }
            let idx = Arc::clone(index);
            let cl = client.clone();
            let env = marker_env.clone();
            let sem = Arc::clone(semaphore);
            let cache = Arc::clone(disk_cache);
            let p = pkg.to_string();
            let metadata_url = format!("{}.metadata", wheel_url);
            tokio::spawn(async move {
                let _claim = claim;
                match fetch_metadata_async(
                    &cl, &metadata_url, &p, &version,
                    &env, &sem,
                ).await {
                    Ok(meta) => {
                        let deps = meta.dependencies.clone();
                        idx.set_metadata(&p, &version, MetadataResult {
                            dependencies: deps,
                            full_metadata: Some(meta.clone()),
                            fetch_error: None,
                        });
                        let v = version.clone();
                        // Awaited so the entry is persisted before exit;
                        // the solver already has the result.
                        let _ = tokio::task::spawn_blocking(move || {
                            cache.lock().expect("poisoned")
                                .insert(&p, &v, &meta);
                        })
                        .await;
                    }
                    Err(e) => {
                        // Never record a failed fetch as "no dependencies".
                        idx.set_metadata(&p, &version, MetadataResult {
                            dependencies: Vec::new(),
                            full_metadata: None,
                            fetch_error: Some(format!("metadata fetch for {}=={} failed: {}", p, version, e)),
                        });
                    }
                }
            });
        }
    }

    /// Fire-and-forget prefetch for a batch of packages. Each package
    /// is handled by `ensure_versions_fetching` which spawns an async
    /// task that fetches versions AND chains metadata for top candidates.
    fn fire_prefetch(&self, packages: &[String]) {
        for pkg_name in packages {
            self.ensure_versions_fetching(pkg_name);
        }
    }
}

// ---------------------------------------------------------------------------
// Async HTTP fetch functions (Simple API v1+JSON only)
// ---------------------------------------------------------------------------

/// Result of a conditional HTTP fetch for version lists.
enum FetchVersionsOutcome {
    /// Server returned fresh data (200 OK).
    Fresh {
        result: VersionsResult,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// Server confirmed the cache is still valid (304 Not Modified).
    NotModified,
}

async fn fetch_versions_async(
    client: &reqwest::Client,
    index_url: &str,
    normalized: &str,
    semaphore: &Semaphore,
    cached_etag: Option<&str>,
    cached_last_modified: Option<&str>,
) -> Result<FetchVersionsOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let _permit = semaphore.acquire().await.expect("semaphore closed");
    let url = format!("{}/{}/", index_url, normalized);

    // Retry transient transport failures and retryable status codes (429/5xx)
    // with exponential backoff before giving up.
    let response = {
        let mut attempt = 0u32;
        loop {
            let mut req = client
                .get(&url)
                .header("Accept", "application/vnd.pypi.simple.v1+json");
            req = crate::auth::apply_auth(req, &url);
            if let Some(etag) = cached_etag {
                req = req.header("If-None-Match", etag);
            }
            if let Some(lm) = cached_last_modified {
                req = req.header("If-Modified-Since", lm);
            }

            match req.send().await {
                Ok(resp) => {
                    if is_retryable_status(resp.status()) && attempt < MAX_FETCH_RETRIES {
                        backoff(attempt).await;
                        attempt += 1;
                        continue;
                    }
                    break resp;
                }
                Err(e) => {
                    if attempt < MAX_FETCH_RETRIES {
                        backoff(attempt).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }
    };

    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(FetchVersionsOutcome::NotModified);
    }

    if !response.status().is_success() {
        let status = response.status();
        // 404/410 mean the package genuinely isn't on this index (empty, no
        // error — lets scoped indexes fall through). Any other non-success
        // status is a transport/server failure the resolver should surface.
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::GONE
        {
            return Ok(FetchVersionsOutcome::Fresh {
                result: VersionsResult {
                    versions: Vec::new(),
                    wheel_info: HashMap::new(),
                    fetch_error: None,
                },
                etag: None,
                last_modified: None,
            });
        }
        return Err(format!("index returned HTTP {} for {}", status, url).into());
    }

    let etag = response
        .headers()
        .get("etag")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    let result = if content_type.contains("json") {
        parse_json_simple_async(response).await?
    } else {
        let body = response.text().await?;
        let versions = parse_html_versions(&body);
        VersionsResult {
            versions,
            wheel_info: HashMap::new(),
            fetch_error: None,
        }
    };

    Ok(FetchVersionsOutcome::Fresh {
        result,
        etag,
        last_modified,
    })
}

async fn parse_json_simple_async(
    response: reqwest::Response,
) -> Result<VersionsResult, Box<dyn std::error::Error + Send + Sync>> {
    // Two-level deserialize: top-level extracts versions eagerly but
    // defers individual file entries as RawValue, then only fully
    // parses wheel files. This avoids allocating structs for the
    // ~80-90% of file entries that are sdists/eggs.
    #[derive(serde::Deserialize)]
    struct SimpleJson<'a> {
        #[serde(default)]
        versions: Vec<String>,
        #[serde(default, borrow)]
        files: Vec<&'a serde_json::value::RawValue>,
    }

    #[derive(serde::Deserialize)]
    struct SimpleFile {
        filename: String,
        #[serde(default)]
        url: String,
        #[serde(default, rename = "data-dist-info-metadata")]
        data_dist_info_metadata: Option<serde_json::Value>,
        #[serde(default, rename = "core-metadata")]
        core_metadata: Option<serde_json::Value>,
        #[serde(default, rename = "requires-python")]
        requires_python: Option<String>,
        /// PEP 592: `false`, `true`, or a string reason. Absent means not yanked.
        #[serde(default)]
        yanked: serde_json::Value,
        #[serde(default)]
        hashes: std::collections::BTreeMap<String, String>,
        /// PEP 700.
        #[serde(default, rename = "upload-time")]
        upload_time: Option<String>,
    }

    let bytes = response.bytes().await?;
    let json: SimpleJson = serde_json::from_slice(&bytes)?;

    let mut version_set = HashSet::new();
    let mut versions = Vec::new();
    let mut wheel_info: HashMap<VypVersion, Vec<WheelInfo>> = HashMap::new();

    if !json.versions.is_empty() {
        for v_str in &json.versions {
            if let Ok(v) = v_str.parse::<VypVersion>() {
                if version_set.insert(v.to_string()) {
                    versions.push(v);
                }
            }
        }
    }

    for raw in &json.files {
        let raw_str = raw.get();
        // Quick string check to skip eggs and other legacy formats without
        // full deserialization. Wheels and sdists are kept.
        if !raw_str.contains(".whl") && !raw_str.contains(".tar.gz") && !raw_str.contains(".zip") {
            if json.versions.is_empty() {
                // Fallback: extract version from any filename
                if let Some(fname_start) = raw_str.find("\"filename\"") {
                    if let Some(v) = extract_version_from_raw_filename(raw_str, fname_start) {
                        if version_set.insert(v.to_string()) {
                            versions.push(v);
                        }
                    }
                }
            }
            continue;
        }

        let file: SimpleFile = match serde_json::from_str(raw_str) {
            Ok(f) => f,
            Err(_) => continue,
        };

        let has_metadata = file.core_metadata.as_ref()
            .or(file.data_dist_info_metadata.as_ref())
            .map(|v| match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::Object(_) => true,
                _ => false,
            })
            .unwrap_or(false);

        // PEP 592: yanked is `true` or a non-empty reason string.
        let yanked = match &file.yanked {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::String(s) => !s.is_empty(),
            _ => false,
        };

        if let Some(file_version) = extract_version_from_filename(&file.filename) {
            if json.versions.is_empty() && version_set.insert(file_version.to_string()) {
                versions.push(file_version.clone());
            }

            wheel_info
                .entry(file_version)
                .or_default()
                .push(WheelInfo {
                    filename: file.filename,
                    url: file.url,
                    has_metadata,
                    requires_python: file.requires_python,
                    yanked,
                    hashes: file.hashes,
                    upload_time: file.upload_time.as_deref().and_then(crate::timestamp::parse_rfc3339),
                });
        }
    }

    versions.sort();
    Ok(VersionsResult {
        versions,
        wheel_info,
        fetch_error: None,
    })
}

/// Quick version extraction from raw JSON without full deserialization.
fn extract_version_from_raw_filename(raw: &str, fname_offset: usize) -> Option<VypVersion> {
    let after = &raw[fname_offset..];
    let colon = after.find(':')?;
    let rest = after[colon + 1..].trim_start();
    if !rest.starts_with('"') { return None; }
    let end = rest[1..].find('"')?;
    let filename = &rest[1..1 + end];
    extract_version_from_filename(filename)
}

/// Fetch METADATA from a Simple API `.metadata` URL and parse `Requires-Dist`.
async fn fetch_metadata_async(
    client: &reqwest::Client,
    metadata_url: &str,
    normalized: &str,
    version: &VypVersion,
    marker_env: &MarkerEnvironment,
    semaphore: &Semaphore,
) -> Result<PackageMetadata, Box<dyn std::error::Error + Send + Sync>> {
    let _permit = semaphore.acquire().await.expect("semaphore closed");
    let response = crate::auth::apply_auth(client.get(metadata_url), metadata_url)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(format!("metadata fetch failed: {}", response.status()).into());
    }

    let text = response.text().await?;
    let dependencies = parse_metadata_requires_dist(&text, marker_env);

    Ok(PackageMetadata {
        package: VypPackage::named(normalized),
        version: version.clone(),
        dependencies,
        conflict_declarations: ConflictSet::new(),
        source: metadata_url.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Pure parsing helpers (no I/O)
// ---------------------------------------------------------------------------

/// Parse RFC 822-style METADATA content to extract `Requires-Dist` entries,
/// filtering by PEP 508 environment markers.
pub fn parse_metadata_requires_dist(content: &str, marker_env: &MarkerEnvironment) -> Vec<Requirement> {
    let mut deps = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("Requires-Dist:") {
            let req_str = rest.trim();
            if let Ok(req) = req_str.parse::<Requirement>() {
                if should_include_requirement(&req, marker_env) {
                    deps.push(req);
                }
            }
        }
    }
    deps
}

fn parse_html_versions(body: &str) -> Vec<VypVersion> {
    let mut versions = Vec::new();
    let mut seen = HashSet::new();

    for line in body.lines() {
        if let Some(href_start) = line.find("href=\"") {
            let rest = &line[href_start + 6..];
            if let Some(href_end) = rest.find('"') {
                let href = &rest[..href_end];
                let filename = href.rsplit('/').next().unwrap_or(href);
                let filename = filename.split('#').next().unwrap_or(filename);
                if let Some(version) = extract_version_from_filename(filename) {
                    if seen.insert(version.to_string()) {
                        versions.push(version);
                    }
                }
            }
        }
    }
    versions
}

fn should_include_requirement(req: &Requirement, env: &MarkerEnvironment) -> bool {
    match &req.marker {
        None => true,
        Some(tree) => tree.evaluate(env, &[]),
    }
}

/// Number of top candidate versions to speculatively prefetch metadata for.
/// Defaults to 2; override with `VYP_PREFETCH_DEPTH` (clamped to 1..=10).
/// The few versions of a package most likely to be picked (the highest ones
/// with PEP 658 metadata), with their wheel URLs. Prefetching only these
/// trades a little latency on heavily constrained resolves for far less
/// wasted bandwidth; the on-demand path covers any miss. Tunable via
/// `VYP_PREFETCH_DEPTH`.
fn speculative_candidates(wheel_info: &HashMap<VypVersion, Vec<WheelInfo>>) -> Vec<(VypVersion, String)> {
    let mut candidates: Vec<(VypVersion, String)> = wheel_info
        .iter()
        .filter_map(|(v, wheels)| {
            wheels
                .iter()
                .find(|w| w.has_metadata && w.filename.ends_with(".whl"))
                .map(|w| (v.clone(), w.url.clone()))
        })
        .collect();
    candidates.sort_by(|(a, _), (b, _)| b.cmp(a));
    candidates.truncate(speculative_prefetch_depth());
    candidates
}

/// Cached metadata, read without holding the cache lock.
fn cached_metadata(
    disk_cache: &Mutex<crate::cache::MetadataCache>,
    pkg: &str,
    version: &VypVersion,
) -> Option<PackageMetadata> {
    let path = disk_cache.lock().expect("poisoned").entry_path(pkg, version)?;
    let meta = crate::cache::MetadataCache::read_entry(&path);
    if meta.is_none() {
        disk_cache.lock().expect("poisoned").forget(pkg, version);
    }
    meta
}

fn speculative_prefetch_depth() -> usize {
    std::env::var("VYP_PREFETCH_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|n| n.clamp(1, 10))
        .unwrap_or(2)
}

/// Pick the URL of the most platform-compatible wheel, falling back to any
/// wheel. A version's `METADATA` is identical across its wheels, so any wheel
/// works as a metadata source for the ranged-fetch fallback.
/// METADATA for an sdist: its static `PKG-INFO` (PEP 643), or a build.
async fn sdist_metadata(
    client: &reqwest::Client,
    sdist: &WheelInfo,
    builder: Option<crate::sdist::SdistMetadataBuilder>,
    sem: &Semaphore,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let path = crate::sdist::download(
        client,
        &sdist.url,
        &sdist.filename,
        sdist.hashes.get("sha256").map(String::as_str),
        sem,
    )
    .await?;
    tokio::task::spawn_blocking(move || {
        if let Ok(pkg_info) = crate::sdist::read_pkg_info(&path) {
            if crate::sdist::has_static_requirements(&pkg_info) {
                return Ok(pkg_info);
            }
        }
        match builder {
            Some(build) => build(&path).map_err(Into::into),
            None => Err("its dependencies are dynamic and building sdists isn't enabled".into()),
        }
    })
    .await?
}

fn best_wheel_url(wheels: &[WheelInfo], env: &MarkerEnvironment) -> Option<String> {
    let tags = PlatformTags::from_env(env);
    let mut best: Option<(u32, &WheelInfo)> = None;
    for w in wheels {
        if !w.filename.ends_with(".whl") || !tags.is_compatible(&w.filename) {
            continue;
        }
        let score = tags.compatibility_score(&w.filename);
        if best.as_ref().is_none_or(|(bs, _)| score > *bs) {
            best = Some((score, w));
        }
    }
    best.map(|(_, w)| w.url.clone()).or_else(|| {
        wheels
            .iter()
            .find(|w| w.filename.ends_with(".whl"))
            .map(|w| w.url.clone())
    })
}

/// Build `PackageMetadata` from raw RFC 822 METADATA text.
fn metadata_from_text(
    text: &str,
    normalized: &str,
    version: &VypVersion,
    env: &MarkerEnvironment,
    source: String,
) -> PackageMetadata {
    PackageMetadata {
        package: VypPackage::named(normalized),
        version: version.clone(),
        dependencies: parse_metadata_requires_dist(text, env),
        conflict_declarations: ConflictSet::new(),
        source,
    }
}

fn dirs_or_default() -> std::path::PathBuf {
    std::env::var("VYP_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_CACHE_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                    std::path::PathBuf::from(home).join(".cache")
                })
        })
}

// ---------------------------------------------------------------------------
// MetadataProvider trait implementation
// ---------------------------------------------------------------------------

impl MetadataProvider for PyPIMetadataProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn priority(&self) -> i32 {
        if self.scope.is_some() {
            20
        } else {
            10
        }
    }

    fn can_provide(&self, package: &VypPackage) -> bool {
        if !matches!(package, VypPackage::Named(_)) {
            return false;
        }
        self.matches_filter(package.name())
    }

    fn available_versions(
        &self,
        package: &VypPackage,
    ) -> Result<Option<PackageVersions>, Box<dyn std::error::Error + Send + Sync>> {
        let normalized = package.name().to_lowercase().replace(['-', '.'], "_");
        // Ensure fetch is in progress (non-blocking spawn).
        self.ensure_versions_fetching(&normalized);
        // Block the solver thread on the Notify until data arrives.
        let result = self.index.wait_versions(&normalized);

        let mut versions = result.versions.clone();
        self.filter_viable(&normalized, &mut versions, &result.wheel_info);

        if versions.is_empty() {
            // Distinguish a real transport failure from "no such package": only
            // the former should abort resolution with an error.
            if let Some(err) = &result.fetch_error {
                return Err(err.clone().into());
            }
            Ok(None)
        } else {
            Ok(Some(PackageVersions {
                package: package.clone(),
                versions,
            }))
        }
    }

    fn get_metadata(
        &self,
        package: &VypPackage,
        version: &VypVersion,
    ) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error + Send + Sync>> {
        let normalized = package.name().to_lowercase().replace(['-', '.'], "_");
        // Ensure fetch is in progress (non-blocking spawn).
        self.ensure_metadata_fetching(&normalized, version);
        // Block the solver thread on the Notify until data arrives.
        let result = self.index.wait_metadata(&normalized, version);
        if let Some(err) = &result.fetch_error {
            return Err(err.clone().into());
        }
        Ok(result.full_metadata.clone())
    }

    fn index_url(&self) -> Option<&str> {
        Some(&self.index_url)
    }

    fn prefetch(&self, packages: &[String]) {
        let to_fetch: Vec<String> = packages
            .iter()
            .map(|p| p.to_lowercase().replace(['-', '.'], "_"))
            .collect();

        if !to_fetch.is_empty() {
            self.fire_prefetch(&to_fetch);
        }
    }

    fn prefetch_metadata(&self, package: &str, versions: &[VypVersion]) {
        let normalized = package.to_lowercase().replace(['-', '.'], "_");

        for version in versions {
            // Only versions with a PEP 658 sidecar are prefetched. Others are
            // left unclaimed so `get_metadata` can still try a ranged wheel
            // read instead of finding an empty result here.
            let has_metadata = self.index
                .get_wheel_info(&normalized, version)
                .map(|wheels| wheels.iter().any(|w| w.has_metadata))
                .unwrap_or(false);
            if !has_metadata {
                continue;
            }

            // Atomically claim. Skip if someone else is already fetching.
            if !self.index.register_metadata(&normalized, version) {
                continue;
            }
            let claim = MetadataClaim::new(Arc::clone(&self.index), &normalized, version);

            let client = self.client.clone();
            let idx = Arc::clone(&self.index);
            let cache = Arc::clone(&self.disk_cache);
            let env = self.marker_env.clone();
            let sem = Arc::clone(&self.semaphore);
            let pkg = normalized.clone();
            let ver = version.clone();

            self.runtime.spawn(async move {
                let _claim = claim;
                if let Some(cached_meta) = cached_metadata(&cache, &pkg, &ver) {
                    idx.set_metadata(&pkg, &ver, MetadataResult {
                        dependencies: cached_meta.dependencies.clone(),
                        full_metadata: Some(cached_meta),
                        fetch_error: None,
                    });
                    return;
                }
                let wheel_url = idx
                    .get_wheel_info(&pkg, &ver)
                    .and_then(|wheels| wheels.iter().find(|w| w.has_metadata).map(|w| w.url.clone()));

                if let Some(wheel_url) = wheel_url {
                    let metadata_url = format!("{}.metadata", wheel_url);
                    if let Ok(meta) = fetch_metadata_async(
                        &client, &metadata_url, &pkg, &ver, &env, &sem,
                    ).await {
                        cache.lock().expect("poisoned").insert(&pkg, &ver, &meta);
                        let deps = meta.dependencies.clone();
                        idx.set_metadata(&pkg, &ver, MetadataResult {
                            dependencies: deps,
                            full_metadata: Some(meta),
                            fetch_error: None,
                        });
                        return;
                    }
                }

                idx.set_metadata(&pkg, &ver, MetadataResult {
                    dependencies: Vec::new(),
                    full_metadata: None,
                    fetch_error: None,
                });
            });
        }
    }

    fn try_available_versions(
        &self,
        package: &VypPackage,
    ) -> Option<PackageVersions> {
        let normalized = package.name().to_lowercase().replace(['-', '.'], "_");
        let result = self.index.try_get_versions(&normalized)?;
        let mut versions = result.versions.clone();
        self.filter_viable(&normalized, &mut versions, &result.wheel_info);
        if versions.is_empty() {
            None
        } else {
            Some(PackageVersions {
                package: package.clone(),
                versions,
            })
        }
    }

    fn wheel_url(
        &self,
        package: &str,
        version: &VypVersion,
    ) -> Option<(String, String)> {
        self.wheel_dist(package, version).map(|d| (d.filename, d.url))
    }

    fn wheel_dist(
        &self,
        package: &str,
        version: &VypVersion,
    ) -> Option<vyp_api::WheelDist> {
        let normalized = package.to_lowercase().replace(['-', '.'], "_");
        let wheels = self.index.get_wheel_info(&normalized, version)?;
        let tags = &self.platform_tags;
        let cutoff = self.exclude_newer.cutoff_for(&normalized);

        let mut best: Option<(u32, &WheelInfo)> = None;
        for w in &wheels {
            if !w.filename.ends_with(".whl")
                || w.yanked
                || !crate::version_filter::uploaded_by(w, cutoff)
            {
                continue;
            }
            if !tags.is_compatible(&w.filename) {
                continue;
            }
            let score = tags.compatibility_score(&w.filename);
            if best.as_ref().is_none_or(|(bs, _)| score > *bs) {
                best = Some((score, w));
            }
        }

        best.map(|(_, w)| vyp_api::WheelDist {
            filename: w.filename.clone(),
            url: w.url.clone(),
            hashes: w.hashes.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            size: None,
        })
    }

    fn sdist_dist(&self, package: &str, version: &VypVersion) -> Option<vyp_api::WheelDist> {
        let normalized = package.to_lowercase().replace(['-', '.'], "_");
        let files = self.index.get_wheel_info(&normalized, version)?;
        let cutoff = self.exclude_newer.cutoff_for(&normalized);
        files
            .iter()
            .filter(|w| {
                crate::version_filter::is_sdist(&w.filename)
                    && !w.yanked
                    && crate::version_filter::uploaded_by(w, cutoff)
            })
            .min_by_key(|w| !w.filename.ends_with(".tar.gz"))
            .map(|w| vyp_api::WheelDist {
                filename: w.filename.clone(),
                url: w.url.clone(),
                hashes: w.hashes.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                size: None,
            })
    }

    fn profile_data(&self) -> HashMap<String, usize> {
        let c = self.profile_counters.lock().expect("poisoned");
        let mut m = HashMap::new();
        m.insert("version_disk_hits".into(), c.version_disk_hits);
        m.insert("version_304s".into(), c.version_304s);
        m.insert("version_fresh_fetches".into(), c.version_fresh_fetches);
        m.insert("metadata_disk_hits".into(), c.metadata_disk_hits);
        m.insert("metadata_network_fetches".into(), c.metadata_network_fetches);
        m
    }
}

// ---------------------------------------------------------------------------
// Filename parsing utilities
// ---------------------------------------------------------------------------

pub fn extract_version_from_filename(filename: &str) -> Option<VypVersion> {
    let name = filename
        .strip_suffix(".whl")
        .or_else(|| filename.strip_suffix(".tar.gz"))
        .or_else(|| filename.strip_suffix(".zip"))
        .unwrap_or(filename);

    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() >= 2 {
        if let Ok(v) = parts[1].parse::<VypVersion>() {
            return Some(v);
        }
    }

    None
}

#[derive(Debug, Clone)]
pub struct WheelFilename {
    pub distribution: String,
    pub version: String,
    pub build_tag: Option<String>,
    pub python_tag: String,
    pub abi_tag: String,
    pub platform_tag: String,
    pub variant_label: Option<String>,
}

impl WheelFilename {
    pub fn parse(filename: &str) -> Option<Self> {
        let stem = filename.strip_suffix(".whl")?;
        let parts: Vec<&str> = stem.split('-').collect();

        match parts.len() {
            5 => Some(WheelFilename {
                distribution: parts[0].to_string(),
                version: parts[1].to_string(),
                build_tag: None,
                python_tag: parts[2].to_string(),
                abi_tag: parts[3].to_string(),
                platform_tag: parts[4].to_string(),
                variant_label: None,
            }),
            6 => {
                if parts[2].starts_with(|c: char| c.is_ascii_digit()) {
                    Some(WheelFilename {
                        distribution: parts[0].to_string(),
                        version: parts[1].to_string(),
                        build_tag: Some(parts[2].to_string()),
                        python_tag: parts[3].to_string(),
                        abi_tag: parts[4].to_string(),
                        platform_tag: parts[5].to_string(),
                        variant_label: None,
                    })
                } else {
                    Some(WheelFilename {
                        distribution: parts[0].to_string(),
                        version: parts[1].to_string(),
                        build_tag: None,
                        python_tag: parts[2].to_string(),
                        abi_tag: parts[3].to_string(),
                        platform_tag: parts[4].to_string(),
                        variant_label: Some(parts[5].to_string()),
                    })
                }
            }
            7 => Some(WheelFilename {
                distribution: parts[0].to_string(),
                version: parts[1].to_string(),
                build_tag: Some(parts[2].to_string()),
                python_tag: parts[3].to_string(),
                abi_tag: parts[4].to_string(),
                platform_tag: parts[5].to_string(),
                variant_label: Some(parts[6].to_string()),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_thread_runtime_reqwest() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let client = reqwest::Client::builder()
            .user_agent("vyp-test/0.1")
            .timeout(std::time::Duration::from_secs(10))
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        let body = rt.block_on(async {
            let resp = client
                .get("https://pypi.org/simple/requests/")
                .header("Accept", "application/vnd.pypi.simple.v1+json")
                .send()
                .await
                .expect("send failed");
            resp.text().await.expect("body failed")
        });
        assert!(body.len() > 100, "got {} bytes", body.len());
    }

    #[test]
    fn test_provider_fetch_versions() {
        let provider = PyPIMetadataProvider::new("https://pypi.org/simple");
        let pkg = VypPackage::named("requests");
        let result = provider.available_versions(&pkg).unwrap();
        assert!(result.is_some(), "expected versions for requests");
        let pv = result.unwrap();
        assert!(!pv.versions.is_empty(), "expected non-empty versions");
        eprintln!("got {} versions for requests", pv.versions.len());
    }

    #[test]
    fn test_extract_version_from_wheel() {
        let v = extract_version_from_filename("numpy-1.26.4-cp312-cp312-macosx_11_0_arm64.whl");
        assert_eq!(v, Some(VypVersion::from_parts(1, 26, 4)));
    }

    #[test]
    fn test_extract_version_from_sdist() {
        let v = extract_version_from_filename("requests-2.31.0.tar.gz");
        assert_eq!(v, Some(VypVersion::from_parts(2, 31, 0)));
    }

    #[test]
    fn test_wheel_filename_parse_standard() {
        let wf =
            WheelFilename::parse("numpy-2.3.2-cp313-cp313t-musllinux_1_2_x86_64.whl").unwrap();
        assert_eq!(wf.distribution, "numpy");
        assert_eq!(wf.version, "2.3.2");
        assert!(wf.build_tag.is_none());
        assert!(wf.variant_label.is_none());
    }

    #[test]
    fn test_wheel_filename_parse_with_variant() {
        let wf = WheelFilename::parse(
            "numpy-2.3.2-cp313-cp313t-musllinux_1_2_x86_64-x86_64_v3.whl",
        )
        .unwrap();
        assert_eq!(wf.distribution, "numpy");
        assert_eq!(wf.variant_label.as_deref(), Some("x86_64_v3"));
        assert!(wf.build_tag.is_none());
    }

    #[test]
    fn test_wheel_filename_parse_with_build_and_variant() {
        let wf = WheelFilename::parse(
            "numpy-2.3.2-1-cp313-cp313t-musllinux_1_2_x86_64-x86_64_v3.whl",
        )
        .unwrap();
        assert_eq!(wf.build_tag.as_deref(), Some("1"));
        assert_eq!(wf.variant_label.as_deref(), Some("x86_64_v3"));
    }

    #[test]
    fn test_cache_dir_is_per_index() {
        let pypi = PyPIMetadataProvider::cache_dir_for("https://pypi.org/simple");
        let torch = PyPIMetadataProvider::cache_dir_for("https://download.pytorch.org/whl/cu128");
        let local = PyPIMetadataProvider::cache_dir_for("http://127.0.0.1:8766/simple");
        assert_eq!(pypi, PyPIMetadataProvider::default_cache_dir());
        assert_ne!(torch, pypi);
        assert_ne!(torch, local);
        assert!(!torch.starts_with(&local) && !local.starts_with(&torch));
    }

    #[test]
    fn test_scope_filter() {
        use std::sync::Mutex;

        // Minimal in-memory scope: an allow-set guarded by a mutex.
        #[derive(Debug)]
        struct SetScope(Mutex<std::collections::HashSet<String>>);
        impl IndexScope for SetScope {
            fn allows(&self, package: &str) -> bool {
                self.0.lock().unwrap().contains(package)
            }
        }

        let set: std::collections::HashSet<String> =
            ["torch".to_string(), "torchvision".to_string()].into_iter().collect();
        let scope: Arc<dyn IndexScope> = Arc::new(SetScope(Mutex::new(set)));
        let provider = PyPIMetadataProvider::with_name(
            "pytorch",
            "https://download.pytorch.org/whl/cu128",
            Some(scope),
        );
        assert!(provider.matches_filter("torch"));
        assert!(provider.matches_filter("torchvision"));
        assert!(!provider.matches_filter("numpy"));
    }

    #[test]
    fn test_unscoped_provider_allows_all() {
        let provider = PyPIMetadataProvider::new("https://pypi.org/simple");
        assert!(provider.matches_filter("anything"));
        assert!(provider.matches_filter("torch"));
    }
}
