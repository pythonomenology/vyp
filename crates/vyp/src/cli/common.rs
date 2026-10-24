use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};
use sha2::{Sha256, Digest};
use vyp_api::{MarkerEnvironment, MarkerTree, Requirement};
use vyp_api::DependencyOverride;
use vyp_core::{ErrataFeedDigest, Erratum, ResolutionResult, ResolverBuilder, ResolveProgress};
use vyp_index::PlatformTags;
use futures::stream::{self, StreamExt};
use rayon::prelude::*;

use crate::cache::wheel_cache::WheelCache;
use crate::cache::linker;
use crate::cache::startup::{self, StartupPolicy};
use crate::cache::venv::VenvLayout;
use crate::config::settings::{InheritOverrides, VypConfig};
use crate::lock::errata::{load_feeds, LoadedErrata};
use crate::lock::shipped_overrides;
use crate::lock::lockfile::{LockFile, PyLockPackage};
use crate::lock::universal::{merge_universal_results, parse_python_version_from_marker, UniversalPackageEntry};

const DEFAULT_INDEX: &str = "https://pypi.org/simple";
const DEFAULT_CONCURRENT_DOWNLOADS: usize = 50;
const MAX_CONCURRENT_INSTALLS: usize = 32;

fn concurrent_downloads() -> usize {
    std::env::var("VYP_CONCURRENT_DOWNLOADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONCURRENT_DOWNLOADS)
}

fn concurrent_installs() -> usize {
    let from_env = std::env::var("VYP_CONCURRENT_INSTALLS")
        .ok()
        .and_then(|s| s.parse().ok());
    if let Some(n) = from_env {
        return n;
    }
    let available = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8);
    available.min(MAX_CONCURRENT_INSTALLS)
}

/// Result of resolution: either a single-environment result or a universal
/// (multi-environment) merged result.
pub enum ResolveOut {
    Single(ResolutionResult),
    Universal {
        entries: Vec<UniversalPackageEntry>,
        environments: Vec<String>,
        errata_feeds: Vec<ErrataFeedDigest>,
        exclude_newer: Option<i64>,
    },
}

/// How many times to re-resolve while overrides shipped by the selected
/// releases keep changing.
const MAX_INHERITANCE_ROUNDS: usize = 4;

/// Run dependency resolution from a `VypConfig`, optionally including
/// extra requirements (e.g. from `vyp add`).
///
/// When `config.environments` is empty, returns `ResolveOut::Single`.
/// When non-empty, resolves once per environment and returns `ResolveOut::Universal`
/// with merged entries and the environment marker list.
pub fn resolve_from_config(
    config: &VypConfig,
    extra_reqs: &[Requirement],
    torch_backend: Option<&str>,
    progress: Option<Box<dyn Fn(ResolveProgress) + Send>>,
) -> miette::Result<(ResolveOut, reqwest::Client)> {
    let shared_client = vyp_index::PyPIMetadataProvider::build_default_client();
    let errata = load_feeds(&config.errata, config.project_dir.as_deref(), &shared_client)?;

    // Builds (path/git source metadata, sdists without static metadata) use
    // the interpreter the resolution targets.
    let frontend = target_python(config).and_then(|python| {
        crate::build::Frontend::for_python(&python, &config.index_url, shared_client.clone()).ok()
    });
    let local_sources = if config.local_sources.is_empty() {
        Vec::new()
    } else {
        let project_dir = config.project_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        crate::build::sources::resolve_all(&config.local_sources, &project_dir, || {
            frontend.clone().ok_or_else(|| {
                miette::miette!("Building local sources needs a Python interpreter; none was found")
            })
        })?
    };
    let sources = ResolveSources { local: &local_sources, frontend: frontend.as_ref() };

    if config.environments.is_empty() {
        let marker_env = match target_python(config) {
            Some(python) => MarkerEnvironment::detect_with(&python.to_string_lossy()),
            None => MarkerEnvironment::detect(),
        };
        let result = resolve_environment(
            config, extra_reqs, torch_backend, &marker_env, &shared_client, &errata, &sources, progress,
        )
        .map_err(|e| match e {
            ResolveError::NoSolution(msg) => miette::miette!("No solution found:\n{}", msg),
            ResolveError::Other(err) => err,
        })?;
        report_applied_fixes(&result, None);
        return Ok((ResolveOut::Single(result), shared_client));
    }

    let mut env_results: Vec<(String, ResolutionResult)> = Vec::new();

    for marker_str in &config.environments {
        let marker_env = parse_python_version_from_marker(marker_str)
            .and_then(|v| MarkerEnvironment::for_python_version_str(&v))
            .ok_or_else(|| {
                miette::miette!(
                    "Unsupported environment marker '{}'; only python_version == \"X.Y\" is supported",
                    marker_str
                )
            })?;

        // Progress callback is not shared across multi-env resolutions (single use per resolve).
        let result = resolve_environment(
            config, extra_reqs, torch_backend, &marker_env, &shared_client, &errata, &sources, None,
        )
        .map_err(|e| match e {
            ResolveError::NoSolution(msg) => miette::miette!("No solution for {}:\n{}", marker_str, msg),
            ResolveError::Other(err) => err,
        })?;
        report_applied_fixes(&result, Some(marker_str));
        env_results.push((marker_str.clone(), result));
    }
    let _ = progress;

    let entries = merge_universal_results(env_results, config.fork_strategy);
    let environments = config.environments.clone();
    Ok((
        ResolveOut::Universal { entries, environments, errata_feeds: errata.feeds, exclude_newer: config.exclude_newer.default },
        shared_client,
    ))
}

/// The interpreter a single-environment resolution targets: the one the
/// command was given, the project's existing environment, or the
/// interpreter a new project environment would be created with.
pub fn target_python(config: &VypConfig) -> Option<PathBuf> {
    if let Some(p) = &config.target_python {
        return Some(p.clone());
    }
    let venv = crate::env::project_venv_path(None, config.project_dir.as_deref());
    let layout_python = VenvLayout::from_root(&venv).ok().map(|l| l.python_exe);
    if let Some(p) = layout_python.filter(|p| p.exists()) {
        return Some(p);
    }
    let request = config
        .python
        .as_deref()
        .map(crate::python::PythonRequest::parse)
        .unwrap_or(crate::python::PythonRequest::Any);
    crate::python::find(&request, config.requires_python.as_deref()).map(|i| i.executable)
}

/// The environment an install targets, created if it doesn't exist yet.
pub fn target_venv(config: &VypConfig, venv: Option<&Path>) -> miette::Result<VenvLayout> {
    crate::env::ensure_project_venv(
        venv,
        config.project_dir.as_deref(),
        config.python.as_deref(),
        config.requires_python.as_deref(),
    )
}

/// Non-index sources for a resolution, prepared once for every environment.
struct ResolveSources<'a> {
    local: &'a [crate::build::sources::ResolvedSource],
    frontend: Option<&'a crate::build::Frontend>,
}

enum ResolveError {
    NoSolution(String),
    Other(miette::Report),
}

impl From<miette::Report> for ResolveError {
    fn from(err: miette::Report) -> Self {
        Self::Other(err)
    }
}

/// Resolve for one target environment, applying errata and any overrides
/// the selected releases ship. Inherited overrides can change which releases
/// are selected, so re-resolve until the set of inherited overrides is stable.
#[allow(clippy::too_many_arguments)]
fn resolve_environment(
    config: &VypConfig,
    extra_reqs: &[Requirement],
    torch_backend: Option<&str>,
    marker_env: &MarkerEnvironment,
    client: &reqwest::Client,
    errata: &LoadedErrata,
    sources: &ResolveSources,
    mut progress: Option<Box<dyn Fn(ResolveProgress) + Send>>,
) -> Result<ResolutionResult, ResolveError> {
    let env_errata = errata.for_environment(marker_env)?;
    let prefetcher = (config.inherit_overrides != InheritOverrides::None)
        .then(|| shipped_overrides::Prefetcher::new(client.clone(), config.inherit_overrides.clone()));
    let mut inherited: Vec<DependencyOverride> = Vec::new();
    let mut seen = Vec::new();

    for _ in 0..MAX_INHERITANCE_ROUNDS {
        let overrides = shipped_overrides::effective_overrides(&config.overrides, &inherited)?;
        let mut result = resolve_once(
            config,
            extra_reqs,
            torch_backend,
            marker_env,
            client,
            overrides,
            env_errata.clone(),
            progress.take(),
            prefetcher.as_ref(),
            sources,
        )?;

        let found = match &prefetcher {
            Some(p) => shipped_overrides::collect(&result, p)?,
            None => Vec::new(),
        };
        let fingerprint = shipped_overrides::fingerprint(&found);
        if fingerprint == shipped_overrides::fingerprint(&inherited) {
            result.errata_feeds = errata.feeds.clone();
            result.exclude_newer = config.exclude_newer.default;
            for source in sources.local {
                if result.packages.contains_key(&source.name) {
                    result.source_refs.insert(source.name.clone(), source.reference.clone());
                }
            }
            return Ok(result);
        }
        if seen.contains(&fingerprint) {
            break;
        }
        seen.push(fingerprint);
        inherited = found;
    }

    Err(ResolveError::Other(miette::miette!(
        "Overrides shipped by dependencies did not settle after {} resolutions: applying them \
         selects releases that ship different overrides. Set inherit-overrides to an allowlist \
         or add [[tool.vyp.overrides]] entries to decide.",
        MAX_INHERITANCE_ROUNDS
    )))
}

#[allow(clippy::too_many_arguments)]
fn resolve_once(
    config: &VypConfig,
    extra_reqs: &[Requirement],
    torch_backend: Option<&str>,
    marker_env: &MarkerEnvironment,
    client: &reqwest::Client,
    overrides: Vec<DependencyOverride>,
    errata: Vec<Erratum>,
    progress: Option<Box<dyn Fn(ResolveProgress) + Send>>,
    prefetcher: Option<&shipped_overrides::Prefetcher>,
    sources: &ResolveSources,
) -> Result<ResolutionResult, ResolveError> {
    let mut config = config.clone();
    if config.sdists != vyp_index::SdistPolicy::Never {
        if let Some(frontend) = sources.frontend.cloned() {
            config.sdist_builder = Some(crate::config::settings::SdistBuilderHook(Arc::new(move |sdist: &Path| {
                // Builds start their own runtimes, which can't nest in the
                // index's async runtime this is called from.
                let frontend = frontend.clone();
                let sdist = sdist.to_path_buf();
                std::thread::spawn(move || {
                    crate::build::unpack_sdist(&sdist)
                        .and_then(|tree| frontend.metadata(&tree))
                        .map_err(|e| e.to_string())
                })
                .join()
                .unwrap_or_else(|_| Err("sdist build panicked".to_string()))
            })));
        }
    }
    let config = &config;
    let mut builder = ResolverBuilder::new()
        .with_overrides(overrides)
        .with_errata(errata)
        .with_substitutions(config.substitutions.clone())
        .with_resolution_strategy(config.core_resolution_strategy())
        .with_pre_release_policy(config.core_pre_release_policy())
        .with_pre_release_packages(config.pre_release_packages.clone());

    if let Some(cb) = progress {
        builder = builder.with_progress(cb);
    }
    if let Some(prefetcher) = prefetcher.cloned() {
        builder = builder.with_selection_hook(Box::new(move |name, _version, dist| {
            prefetcher.start(name, &dist.url, dist.hashes.get("sha256").map(String::as_str));
        }));
    }

    config.load_plugins(builder.plugin_loader_mut());

    for req_str in &config.dependencies {
        let req: Requirement = req_str
            .parse()
            .map_err(|e| miette::miette!("Invalid requirement '{}': {}", req_str, e))?;
        builder = builder.add_dependency(req);
    }

    for req in extra_reqs {
        builder = builder.add_dependency(req.clone());
    }

    let (providers, router) = config.create_providers_with_client(
        torch_backend,
        Some(marker_env),
        Some(client.clone()),
    )?;
    for provider in providers {
        builder = builder.with_provider(provider);
    }
    if !sources.local.is_empty() {
        builder = builder.with_provider(Box::new(crate::build::sources::DirectProvider::new(sources.local, marker_env)));
    }
    builder = builder.with_index_router(router);

    builder.resolve().map_err(|e| match e {
        vyp_core::VypError::NoSolution(msg) => ResolveError::NoSolution(msg),
        other => ResolveError::Other(miette::miette!("Resolution failed: {}", other)),
    })
}

/// Tell the user about corrections they did not write themselves: overrides
/// shipped by dependencies and errata that touched the selected releases.
fn report_applied_fixes(result: &ResolutionResult, environment: Option<&str>) {
    let suffix = environment.map(|e| format!(" ({})", e)).unwrap_or_default();
    let inherited: Vec<_> = result.overrides.iter().filter(|o| o.origin.is_some()).collect();
    if !inherited.is_empty() {
        eprintln!("Applied {} override(s) shipped by dependencies{}:", inherited.len(), suffix);
        for o in inherited {
            eprintln!(
                "  {} {} (from {})",
                o.package,
                o.constraint,
                shipped_overrides::describe_origin(o)
            );
        }
    }
    for warning in &result.backtracks {
        eprintln!("warning{}: {}", suffix, warning.describe());
    }
    let touched = result
        .errata
        .iter()
        .filter(|e| {
            result.packages.get(&e.package).is_some_and(|v| e.exclude || e.applies_to(&e.package, v))
        })
        .count();
    if touched > 0 {
        eprintln!("Applied {} erratum/errata to selected packages{}", touched, suffix);
    }
}

/// Timing breakdown of the install phase (populated when VYP_PROFILE=1).
/// All _ms fields are wall-clock except download/extract/link which are summed across workers.
pub struct InstallTiming {
    pub marker_detect_ms: f64,
    pub site_packages_ms: f64,
    pub cache_check_ms: f64,
    pub runtime_client_ms: f64,
    pub pipeline_wall_ms: f64,
    pub cached_link_wall_ms: f64,
    pub download_ms: f64,
    pub extract_ms: f64,
    pub link_ms: f64,
    pub eviction_ms: f64,
    pub total_ms: f64,
    pub cached_count: usize,
    pub download_count: usize,
    pub link_count: usize,
}

struct DownloadedWheel {
    #[allow(dead_code)]
    pkg_name: String,
    #[allow(dead_code)]
    pkg_version: String,
    path: PathBuf,
    sha256: String,
}

/// Install packages from a lock file into a virtual environment.
///
/// Architecture: streaming pipeline — all phases overlap:
///   1. Check wheel cache for pre-extracted archives
///   2. Link cached packages AND download missing wheels **concurrently**
///   3. As EACH download completes, immediately extract+link (no waiting
///      for all downloads to finish first)
///
/// Wall-clock time ≈ max(download) + extract_tail + link_tail, not sum(all).
/// Filter lockfile packages to those applicable for the current marker environment.
fn packages_for_env<'a>(
    packages: &'a [PyLockPackage],
    marker_env: &MarkerEnvironment,
) -> Vec<&'a PyLockPackage> {
    packages
        .iter()
        .filter(|pkg| {
            match &pkg.marker {
                None => true,
                Some(m) => MarkerTree::parse(m).evaluate(marker_env, &[]),
            }
        })
        .collect()
}

/// How `install_lockfile` behaves.
#[derive(Clone, Default)]
pub struct InstallOptions {
    pub dry_run: bool,
    /// Index to look up wheel URLs on when the lock doesn't carry one.
    pub default_index: Option<String>,
    pub client: Option<reqwest::Client>,
    pub startup: StartupPolicy,
    /// Suppress progress output (build environments).
    pub quiet: bool,
    /// Directory that relative `directory` paths in the lock are relative to.
    pub lock_dir: Option<PathBuf>,
}

pub fn install_lockfile(
    lockfile: &LockFile,
    layout: &VenvLayout,
    options: &InstallOptions,
) -> miette::Result<Option<InstallTiming>> {
    let dry_run = options.dry_run;
    let default_index = options.default_index.as_deref();
    let shared_client = options.client.clone();
    let startup_policy = &options.startup;
    let quiet = options.quiet;
    let profiling = std::env::var("VYP_PROFILE").is_ok_and(|v| v == "1");
    crate::env::provenance::preflight(layout)?;
    let t_marker_start = Instant::now();
    let marker_env = MarkerEnvironment::detect_with(&layout.python_exe.to_string_lossy());
    let marker_detect_ms = if profiling { t_marker_start.elapsed().as_secs_f64() * 1000.0 } else { 0.0 };
    let packages = packages_for_env(&lockfile.packages, &marker_env);
    // Local trees, VCS checkouts and sdist-only releases are built first,
    // then installed like any wheel.
    let (to_build, packages): (Vec<&PyLockPackage>, Vec<&PyLockPackage>) =
        packages.into_iter().partition(|p| needs_build(p));

    if packages.is_empty() && to_build.is_empty() {
        println!("No packages to install for this environment.");
        return Ok(None);
    }

    let t_site_start = Instant::now();
    let layout = layout.clone();
    let site_packages = layout.site_packages.clone();
    let site_packages_ms = if profiling { t_site_start.elapsed().as_secs_f64() * 1000.0 } else { 0.0 };
    let fallback_index = default_index.unwrap_or(DEFAULT_INDEX);

    if dry_run {
        println!(
            "Would install {} packages into {}:",
            packages.len(),
            site_packages.display()
        );
        for pkg in packages.iter().chain(&to_build) {
            println!("  {} == {}{}", pkg.name, pkg.version, if needs_build(pkg) { " (build)" } else { "" });
        }
        return Ok(None);
    }

    let total = packages.len() + to_build.len();
    if !quiet {
        println!("Installing {} packages into {}...", total, site_packages.display());
    }

    let start = Instant::now();
    let wheel_cache = WheelCache::new();
    let platform_tags = PlatformTags::from_env(&marker_env);

    // Phase 1: Check cache, partition into cached vs needs-download.
    let phase1_start = Instant::now();
    let mut cached_archives: Vec<(&PyLockPackage, PathBuf, Option<Vec<PathBuf>>)> = Vec::new();
    let mut to_download: Vec<&PyLockPackage> = Vec::new();

    for pkg in &packages {
        let key = pkg_cache_key(pkg);
        if let Some((archive_path, file_list)) = wheel_cache.get_archive_with_file_list(&key) {
            cached_archives.push((pkg, archive_path, file_list));
            continue;
        }
        to_download.push(pkg);
    }
    let cache_check_ms = phase1_start.elapsed().as_secs_f64() * 1000.0;
    let cached_count = cached_archives.len();
    let download_count = to_download.len();

    // Skip per-file exists/remove when site_packages is empty (fresh install).
    let assume_fresh = !site_packages.exists()
        || std::fs::read_dir(&site_packages)
            .map(|d| d.count() == 0)
            .unwrap_or(false);
    let assume_fresh_dl = assume_fresh && cached_count == 0;

    let dl_ns = Arc::new(AtomicUsize::new(0));
    let ext_ns = Arc::new(AtomicUsize::new(0));
    let lnk_ns = Arc::new(AtomicUsize::new(0));
    let cached_link_wall_ns = Arc::new(AtomicU64::new(0));

    let t_runtime_start = Instant::now();
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_workers)
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("Failed to start async runtime: {}", e))?;

    let concurrent_dl = concurrent_downloads();
    let install_workers = concurrent_installs();
    let client = shared_client.unwrap_or_else(|| {
        reqwest::Client::builder()
            .user_agent(format!("vyp/{}", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(concurrent_dl)
            .tcp_nodelay(true)
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to create HTTP client")
    });
    let runtime_client_ms = if profiling { t_runtime_start.elapsed().as_secs_f64() * 1000.0 } else { 0.0 };

    let tmp_dir = wheel_cache.tmp_dir();
    let _ = std::fs::create_dir_all(&tmp_dir);

    // Collect owned data for the cache link task (needs 'static for spawn_blocking).
    let cached_owned: Vec<(String, PathBuf, Option<Vec<PathBuf>>)> = cached_archives
        .iter()
        .map(|(pkg, path, list)| (pkg.name.clone(), path.clone(), list.clone()))
        .collect();

    // Collect owned download data (needs 'static for async closures).
    struct DownloadJob {
        name: String,
        version: String,
        wheels: Vec<crate::lock::lockfile::PyLockWheel>,
    }
    let download_jobs: Vec<DownloadJob> = to_download
        .iter()
        .map(|pkg| DownloadJob {
            name: pkg.name.clone(),
            version: pkg.version.clone(),
            wheels: pkg.wheels.clone(),
        })
        .collect();

    let tags_owned = platform_tags.clone();

    // Run cached link + streaming download→extract→link concurrently.
    let t_pipeline_start = Instant::now();
    let (link_cached_results, download_pipeline_results) = rt.block_on(async {
        let lnk_c = Arc::clone(&lnk_ns);
        let cached_link_wall = Arc::clone(&cached_link_wall_ns);
        let assume_fresh = assume_fresh;
        let install_workers = install_workers;

        let layout_cached = layout.clone();
        let policy_cached = startup_policy.clone();
        let cache_link_handle = tokio::task::spawn_blocking(move || {
            let t = Instant::now();
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(install_workers)
                .build()
                .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().num_threads(1).build().expect("fallback pool"));
            let results: Vec<_> = pool.install(|| {
                cached_owned
                    .par_iter()
                    .map(|(name, archive_path, file_list)| {
                        let t = Instant::now();
                        let files = archive_files(archive_path, file_list.as_deref());
                        match startup::check(name, archive_path, &files, &policy_cached) {
                            Err(e) => return Err((name.clone(), e)),
                            Ok(Some(warning)) => eprintln!("warning: {}", warning),
                            Ok(None) => {}
                        }
                        let list_ref = file_list.as_deref();
                        let res = linker::install_wheel(archive_path, &layout_cached, list_ref, assume_fresh, false)
                            .map_err(|e| (name.clone(), format!("install failed: {}", e)));
                        lnk_c.fetch_add(t.elapsed().as_nanos() as usize, AtomicOrdering::Relaxed);
                        res
                    })
                    .collect()
            });
            cached_link_wall.store(t.elapsed().as_nanos() as u64, AtomicOrdering::Relaxed);
            results
        });

        let pipeline_results: Vec<Result<String, (String, String)>> = if download_jobs.is_empty() {
            Vec::new()
        } else {
            let assume_fresh_dl = assume_fresh_dl;
            stream::iter(download_jobs.into_iter())
                .map(|job| {
                    let cl = client.clone();
                    let fb = fallback_index.to_string();
                    let tmp = tmp_dir.clone();
                    let layout2 = layout.clone();
                    let tags = tags_owned.clone();
                    let dl_c = Arc::clone(&dl_ns);
                    let ext_c = Arc::clone(&ext_ns);
                    let lnk_c = Arc::clone(&lnk_ns);
                    let assume_fresh_dl = assume_fresh_dl;
                    let policy = startup_policy.clone();
                    async move {
                        let dl_start = Instant::now();
                        let dw = match download_wheel_job(&cl, &job.name, &job.version, &job.wheels, &fb, &tmp, &tags).await {
                            Ok(dw) => dw,
                            Err(e) => return Err((job.name.clone(), e.to_string())),
                        };
                        let dl_elapsed = dl_start.elapsed().as_nanos() as usize;

                        let expected_hash = job.wheels.first()
                            .and_then(|w| w.hashes.as_ref())
                            .and_then(|h| h.get("sha256"))
                            .cloned();
                        let cache_key = format!("{}-{}", job.name.to_lowercase().replace('-', "_"), job.version);
                        let pkg_name = job.name;
                        let job_version = job.version;

                        tokio::task::spawn_blocking(move || {
                            dl_c.fetch_add(dl_elapsed, AtomicOrdering::Relaxed);

                            if let Some(expected) = &expected_hash {
                                if dw.sha256 != *expected {
                                    return Err((
                                        pkg_name,
                                        format!("hash mismatch: expected {}, got {}", expected, dw.sha256),
                                    ));
                                }
                            }

                            let wc = WheelCache::new();
                            let ext_start = Instant::now();
                            let (archive_path, file_list) = match wc.store_from_file_with_list(&cache_key, &dw.path, false) {
                                Ok(r) => r,
                                Err(e) => return Err((pkg_name, format!("cache store failed: {}", e))),
                            };
                            let _ = std::fs::remove_file(&dw.path);
                            ext_c.fetch_add(ext_start.elapsed().as_nanos() as usize, AtomicOrdering::Relaxed);

                            // With no published hash to pin, fall back to a
                            // name/version identity check against the wheel's
                            // own METADATA (guards against index confusion).
                            if expected_hash.is_none() {
                                if let Err(e) = linker::verify_wheel_identity(&archive_path, &pkg_name, &job_version) {
                                    return Err((pkg_name, e.to_string()));
                                }
                            }

                            let files = archive_files(&archive_path, (!file_list.is_empty()).then_some(file_list.as_slice()));
                            match startup::check(&pkg_name, &archive_path, &files, &policy) {
                                Err(e) => return Err((pkg_name, e)),
                                Ok(Some(warning)) => eprintln!("warning: {}", warning),
                                Ok(None) => {}
                            }

                            let link_start = Instant::now();
                            let file_list_ref = if file_list.is_empty() { None } else { Some(file_list.as_slice()) };
                            if let Err(e) = linker::install_wheel(&archive_path, &layout2, file_list_ref, assume_fresh_dl, false) {
                                return Err((pkg_name, format!("install failed: {}", e)));
                            }
                            lnk_c.fetch_add(link_start.elapsed().as_nanos() as usize, AtomicOrdering::Relaxed);

                            Ok(pkg_name)
                        })
                        .await
                        .unwrap_or_else(|e| Err(("unknown".into(), format!("task panicked: {}", e))))
                    }
                })
                .buffer_unordered(concurrent_dl)
                .collect::<Vec<_>>()
                .await
        };

        let cache_results = cache_link_handle.await
            .unwrap_or_else(|e| vec![Err(("cache-link".into(), format!("task panicked: {}", e)))]);

        (cache_results, pipeline_results)
    });
    let pipeline_wall_ms = t_pipeline_start.elapsed().as_secs_f64() * 1000.0;

    let mut installed = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();

    if !to_build.is_empty() {
        let lock_dir = options
            .lock_dir
            .clone()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from("."));
        let frontend = crate::build::Frontend::for_python(&layout.python_exe, fallback_index, client.clone())?;
        for pkg in &to_build {
            match build_and_install(pkg, &frontend, &layout, &lock_dir, &rt, &client, &wheel_cache, startup_policy, quiet) {
                Ok(()) => installed += 1,
                Err(e) => failed.push((pkg.name.clone(), e.to_string())),
            }
        }
    }

    for r in link_cached_results {
        match r {
            Ok(()) => installed += 1,
            Err(e) => failed.push(e),
        }
    }
    for r in download_pipeline_results {
        match r {
            Ok(_) => installed += 1,
            Err(e) => failed.push(e),
        }
    }

    let t_evict_start = Instant::now();
    if download_count > 0 {
        wheel_cache.evict_archives_if_needed();
    }
    let eviction_ms = if profiling { t_evict_start.elapsed().as_secs_f64() * 1000.0 } else { 0.0 };

    let elapsed = start.elapsed();
    let total_ms = elapsed.as_secs_f64() * 1000.0;
    if !quiet {
        println!(
            "\nInstalled {} package(s) in {:.2}s.",
            installed,
            elapsed.as_secs_f64()
        );
    }
    if !failed.is_empty() {
        println!("{} package(s) failed:", failed.len());
        for (name, err) in &failed {
            println!("  {}: {}", name, err);
        }
        return Err(miette::miette!(
            "{} package(s) failed to install",
            failed.len()
        ));
    }

    let timing = if profiling {
        let download_ms = dl_ns.load(AtomicOrdering::Relaxed) as f64 / 1_000_000.0;
        let extract_ms = ext_ns.load(AtomicOrdering::Relaxed) as f64 / 1_000_000.0;
        let link_ms = lnk_ns.load(AtomicOrdering::Relaxed) as f64 / 1_000_000.0;
        let cached_link_wall_ms = cached_link_wall_ns.load(AtomicOrdering::Relaxed) as f64 / 1_000_000.0;
        Some(InstallTiming {
            marker_detect_ms,
            site_packages_ms,
            cache_check_ms,
            runtime_client_ms,
            pipeline_wall_ms,
            cached_link_wall_ms,
            download_ms,
            extract_ms,
            link_ms,
            eviction_ms,
            total_ms,
            cached_count,
            download_count,
            link_count: installed,
        })
    } else {
        None
    };

    Ok(timing)
}

fn needs_build(pkg: &PyLockPackage) -> bool {
    pkg.directory.is_some() || pkg.vcs.is_some() || (pkg.wheels.is_empty() && pkg.sdist.is_some())
}

/// Build a package that has no wheel in the lock and install the result.
#[allow(clippy::too_many_arguments)]
fn build_and_install(
    pkg: &PyLockPackage,
    frontend: &crate::build::Frontend,
    layout: &VenvLayout,
    lock_dir: &Path,
    rt: &tokio::runtime::Runtime,
    client: &reqwest::Client,
    wheel_cache: &WheelCache,
    startup_policy: &StartupPolicy,
    quiet: bool,
) -> miette::Result<()> {
    let out_dir = crate::build::scratch_dir("wheel")?;
    let (wheel, own_code) = if let Some(dir) = &pkg.directory {
        let tree = lock_dir.join(&dir.path);
        let tree = match &dir.subdirectory {
            Some(sub) => tree.join(sub),
            None => tree,
        };
        if !quiet {
            println!("Building {} from {}{}", pkg.name, tree.display(), if dir.editable { " (editable)" } else { "" });
        }
        (frontend.build(&tree, dir.editable, &out_dir)?, true)
    } else if let Some(vcs) = &pkg.vcs {
        if vcs.kind != "git" {
            return Err(miette::miette!("{} is a {} checkout; only git is supported", pkg.name, vcs.kind));
        }
        let checkout = crate::build::git::checkout(&vcs.url, &vcs.commit_id)?;
        let tree = match &vcs.subdirectory {
            Some(sub) => checkout.join(sub),
            None => checkout,
        };
        if !quiet {
            println!("Building {} from {}@{}", pkg.name, vcs.url, &vcs.commit_id[..vcs.commit_id.len().min(12)]);
        }
        (cached_build(frontend, &format!("git-{}-{:?}", vcs.commit_id, vcs.subdirectory), &tree, &out_dir)?, false)
    } else if let Some(sdist) = &pkg.sdist {
        let url = sdist
            .url
            .clone()
            .ok_or_else(|| miette::miette!("The lock has no URL for {}'s sdist", pkg.name))?;
        let sha = sdist.hashes.as_ref().and_then(|h| h.get("sha256")).cloned();
        let semaphore = tokio::sync::Semaphore::new(1);
        let path = rt
            .block_on(vyp_index::sdist::download(client, &url, &sdist.name, sha.as_deref(), &semaphore))
            .map_err(|e| miette::miette!("Failed to download {}: {}", sdist.name, e))?;
        if !quiet {
            println!("Building {} from {}", pkg.name, sdist.name);
        }
        let key = format!("sdist-{}", sha.unwrap_or_else(|| crate::python::hex_sha256(url.as_bytes())));
        let tree = crate::build::unpack_sdist(&path)?;
        (cached_build(frontend, &key, &tree, &out_dir)?, false)
    } else {
        return Err(miette::miette!("Nothing to build for {}", pkg.name));
    };

    let wheel_bytes = std::fs::read(&wheel).map_err(|e| miette::miette!("Cannot read {}: {}", wheel.display(), e))?;
    let key = format!(
        "built-{}-{}-{}",
        vyp_api::normalize_package_name(&pkg.name),
        pkg.version,
        &crate::python::hex_sha256(&wheel_bytes)[..16]
    );
    let (archive, list) = wheel_cache
        .store_from_file_with_list(&key, &wheel, false)
        .map_err(|e| miette::miette!("Failed to unpack {}: {}", wheel.display(), e))?;
    let _ = std::fs::remove_dir_all(&out_dir);
    let files = archive_files(&archive, (!list.is_empty()).then_some(list.as_slice()));

    // The project's own source trees (including editable installs, which
    // use a .pth file by design) aren't third-party code.
    if !own_code {
        match startup::check(&pkg.name, &archive, &files, startup_policy) {
            Err(e) => return Err(miette::miette!("{}", e)),
            Ok(Some(warning)) => eprintln!("warning: {}", warning),
            Ok(None) => {}
        }
    }
    // A rebuilt tree can drop files the previous build installed.
    if let Some(existing) = crate::cache::installed::find_installed(layout, &pkg.name) {
        crate::cache::installed::uninstall_dist(layout, &existing)
            .map_err(|e| miette::miette!("Failed to remove the previous {}: {}", pkg.name, e))?;
    }
    linker::install_wheel(&archive, layout, Some(&files), false, false)
        .map_err(|e| miette::miette!("Failed to install {}: {}", pkg.name, e))
}

/// Build a wheel once per (immutable) source and interpreter, reusing it after.
fn cached_build(
    frontend: &crate::build::Frontend,
    key: &str,
    tree: &Path,
    out_dir: &Path,
) -> miette::Result<PathBuf> {
    let dir = crate::cache::wheel_cache::cache_base_dir()
        .join("built-wheels")
        .join(&crate::python::hex_sha256(format!("{}\n{}", key, frontend.interpreter_key()).as_bytes())[..24]);
    if let Some(wheel) = std::fs::read_dir(&dir)
        .ok()
        .and_then(|d| d.flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "whl")))
    {
        return Ok(wheel);
    }
    let built = frontend.build(tree, false, out_dir)?;
    std::fs::create_dir_all(&dir).map_err(|e| miette::miette!("{}", e))?;
    let dest = dir.join(built.file_name().unwrap_or_default());
    std::fs::copy(&built, &dest).map_err(|e| miette::miette!("Failed to cache {}: {}", built.display(), e))?;
    Ok(dest)
}

/// An extracted wheel's relative file list, walking the directory when the
/// cache has no manifest for it.
fn archive_files(archive: &Path, list: Option<&[PathBuf]>) -> Vec<PathBuf> {
    match list {
        Some(l) => l.to_vec(),
        None => walkdir::WalkDir::new(archive)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| e.path().strip_prefix(archive).ok().map(Path::to_path_buf))
            .collect(),
    }
}

/// Stable cache key for a package: normalized name + version.
fn pkg_cache_key(pkg: &PyLockPackage) -> String {
    format!("{}-{}", pkg.name.to_lowercase().replace('-', "_"), pkg.version)
}

/// Download a wheel given owned job data (for the streaming pipeline).
async fn download_wheel_job(
    client: &reqwest::Client,
    name: &str,
    version: &str,
    wheels: &[crate::lock::lockfile::PyLockWheel],
    fallback_index: &str,
    tmp_dir: &Path,
    tags: &PlatformTags,
) -> Result<DownloadedWheel, Box<dyn std::error::Error + Send + Sync>> {
    let url = {
        let lockfile_url = wheels.first().and_then(|w| w.url.as_ref()).cloned();
        let compatible = lockfile_url.as_ref().is_some_and(|u| {
            let fname = u.rsplit('/').next().unwrap_or(u);
            let fname = fname.split('#').next().unwrap_or(fname);
            !fname.ends_with(".whl") || tags.is_compatible(fname)
        });
        if compatible {
            lockfile_url.unwrap()
        } else {
            let normalized = name.to_lowercase().replace('-', "_");
            resolve_wheel_url_direct(client, &normalized, version, fallback_index, tags).await?
        }
    };

    let response = vyp_index::auth::apply_auth(client.get(&url), &url)
        .send()
        .await?
        .error_for_status()?;

    let tmp_path = tmp_dir.join(format!(
        "{}-{}.whl.tmp",
        name.to_lowercase().replace('-', "_"),
        version
    ));
    let mut file = tokio::fs::File::create(&tmp_path).await?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();

    use futures::TryStreamExt;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.try_next().await? {
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;

    let sha256 = format!("{:x}", hasher.finalize());

    Ok(DownloadedWheel {
        pkg_name: name.to_string(),
        pkg_version: version.to_string(),
        path: tmp_path,
        sha256,
    })
}

/// Resolve wheel URL using normalized name and version directly.
async fn resolve_wheel_url_direct(
    client: &reqwest::Client,
    normalized_name: &str,
    version: &str,
    index_url: &str,
    tags: &PlatformTags,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!(
        "{}/{}/",
        index_url.trim_end_matches('/'),
        normalized_name.replace('_', "-")
    );

    let response = vyp_index::auth::apply_auth(
        client
            .get(&url)
            .header("Accept", "application/vnd.pypi.simple.v1+json"),
        &url,
    )
    .send()
    .await?
    .error_for_status()?;

    let body = response.text().await?;

    if let Ok(index) = serde_json::from_str::<serde_json::Value>(&body) {
        find_wheel_url_json(&index, normalized_name, version, tags)
    } else {
        find_wheel_url_html(&body, version, tags)
    }
}

fn find_wheel_url_json(
    index: &serde_json::Value,
    name: &str,
    version: &str,
    tags: &PlatformTags,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let files = index
        .get("files")
        .and_then(|f| f.as_array())
        .ok_or("No files in index response")?;

    let version_tag = format!("-{}-", version);

    let mut best: Option<(u32, &str)> = None;
    for f in files {
        let filename = match f.get("filename").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => continue,
        };
        if !filename.contains(&version_tag) || !filename.ends_with(".whl") {
            continue;
        }
        if !tags.is_compatible(filename) {
            continue;
        }
        let url = match f.get("url").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => continue,
        };
        let score = tags.compatibility_score(filename);
        if best.as_ref().is_none_or(|(bs, _)| score > *bs) {
            best = Some((score, url));
        }
    }

    best.map(|(_, url)| url.to_string())
        .ok_or_else(|| format!("No compatible wheel found for {}=={}", name, version).into())
}

fn find_wheel_url_html(
    body: &str,
    version: &str,
    tags: &PlatformTags,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let version_tag = format!("-{}-", version);
    let mut best: Option<(u32, String)> = None;

    for line in body.lines() {
        if let Some(href_start) = line.find("href=\"") {
            let rest = &line[href_start + 6..];
            if let Some(href_end) = rest.find('"') {
                let href = &rest[..href_end];
                let filename = href.rsplit('/').next().unwrap_or(href);
                let filename = filename.split('#').next().unwrap_or(filename);
                if filename.contains(&version_tag) && filename.ends_with(".whl") && tags.is_compatible(filename) {
                    let score = tags.compatibility_score(filename);
                    if best.as_ref().is_none_or(|(bs, _)| score > *bs) {
                        best = Some((score, href.to_string()));
                    }
                }
            }
        }
    }

    best.map(|(_, url)| url)
        .ok_or_else(|| format!("No compatible wheel found for version {} in HTML index", version).into())
}

