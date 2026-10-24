pub mod errata;
pub mod index_router;
pub mod plugin;
pub mod resolver;
pub mod strategies;

pub use errata::{ErrataFeedDigest, Erratum};
pub use index_router::IndexRouter;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use version_ranges::Ranges;

use vyp_api::{
    VypPackage, VypVersion, DependencyOverride, Requirement,
    ResolutionProvenance, SubstitutionSet, SelectionReason,
    ConflictSet, normalize_package_name,
};
use vyp_api::traits::conflict_strategy::{ConflictContext, StrategyVerdict};
use vyp_api::traits::resolution_filter::Candidate;

use plugin::loader::PluginLoader;
use plugin::registry::{FilterRegistry, ProviderRegistry, StrategyRegistry};
use resolver::{requirements_to_range, SolverState, SolverError, PackageId, VS};
use strategies::override_strategy::OverrideConflictStrategy;

type DepsCache = HashMap<(String, VypVersion), Arc<Vec<(VypPackage, VS)>>>;
/// Normalized package name -> the range every requirement on it is replaced with.
type OverrideRanges = HashMap<String, VS>;
use strategies::substitution::{SubstitutionFilter, SubstitutionStrategy};
use strategies::transitive_fork::TransitiveForkStrategy;

/// Errors from the Vyp resolver.
#[derive(Debug, thiserror::Error)]
pub enum VypError {
    #[error("no solution found: {0}")]
    NoSolution(String),
    #[error("resolution error: {0}")]
    ResolutionError(String),
    #[error("plugin error: {0}")]
    PluginError(String),
}

/// Result of a successful resolution.
#[derive(Debug)]
pub struct ResolutionResult {
    /// Selected packages and their versions.
    pub packages: HashMap<String, VypVersion>,
    /// Provenance tracking data.
    pub provenance: ResolutionProvenance,
    /// Conflict declarations collected during resolution.
    pub inherited_conflicts: HashMap<String, ConflictSet>,
    /// Best wheel URL for each resolved package (keyed by normalized name).
    pub wheel_urls: HashMap<String, WheelUrlInfo>,
    /// The sdist to build, for resolved packages with no compatible wheel.
    pub sdist_urls: HashMap<String, WheelUrlInfo>,
    /// Timing breakdown (populated when `VYP_PROFILE=1`).
    pub timing: Option<ResolveTiming>,
    /// Overrides in effect for this resolution, including inherited ones.
    pub overrides: Vec<DependencyOverride>,
    /// Errata in effect for this resolution.
    pub errata: Vec<Erratum>,
    /// Errata feeds consulted, with the digest of the content used. Filled in
    /// by the caller that loaded the feeds.
    pub errata_feeds: Vec<ErrataFeedDigest>,
    /// Packages that come from a local directory or a VCS checkout instead
    /// of an index. Filled in by the caller that supplied those sources.
    pub source_refs: HashMap<String, SourceRef>,
    /// Packages that fell back to an older release because their newer
    /// releases added an upper bound the selected dependency violates.
    pub backtracks: Vec<BacktrackWarning>,
    /// The upload-time cutoff (Unix seconds) the indexes were filtered by.
    /// Filled in by the caller that configured the providers.
    pub exclude_newer: Option<i64>,
}

/// Where a package that isn't from an index comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRef {
    /// A local source tree; `path` is relative to the project when possible.
    Directory { path: String, editable: bool },
    Git {
        url: String,
        requested_revision: Option<String>,
        commit: String,
        subdirectory: Option<String>,
    },
}

/// A package resolved to an older release because every newer one excludes
/// the version of a dependency that was selected, while the chosen release
/// declares no such limit. It installs, but likely has the same
/// incompatibility the newer releases fixed by adding the bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacktrackWarning {
    pub package: String,
    pub selected: VypVersion,
    pub newest: VypVersion,
    /// Viable releases between the selected and the newest.
    pub skipped: usize,
    pub dependency: String,
    pub dependency_version: VypVersion,
    /// The newest release's requirement on `dependency`, e.g. `<2`.
    pub newest_requires: String,
}

impl BacktrackWarning {
    pub fn describe(&self) -> String {
        format!(
            "{pkg} {sel} was selected instead of {pkg} {new} ({skipped} newer release{s} skipped): \
             {pkg} {new} requires {dep}{req} but {dep} {dv} is selected. {pkg} {sel} doesn't declare \
             that limit, so it may be just as incompatible with {dep} {dv}.",
            pkg = self.package,
            sel = self.selected,
            new = self.newest,
            skipped = self.skipped,
            s = if self.skipped == 1 { "" } else { "s" },
            dep = self.dependency,
            req = self.newest_requires,
            dv = self.dependency_version,
        )
    }
}

/// Timing breakdown of the resolve phase.
#[derive(Debug, Clone)]
pub struct ResolveTiming {
    pub total_ms: f64,
    pub version_wait_ms: f64,
    pub metadata_wait_ms: f64,
    pub solver_ms: f64,
    pub wheel_url_ms: f64,
    pub iterations: usize,
    pub version_fetches: usize,
    pub metadata_fetches: usize,
    pub provider_counters: HashMap<String, usize>,
}

/// Wheel download information discovered during resolution.
#[derive(Debug, Clone)]
pub struct WheelUrlInfo {
    pub filename: String,
    pub url: String,
    /// Integrity hashes published by the index (algo -> hex digest), pinned
    /// into the lock file for supply-chain verification at install time.
    pub hashes: std::collections::BTreeMap<String, String>,
}

/// Controls how versions are sorted during resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResolutionStrategy {
    #[default]
    Highest,
    Lowest,
    LowestDirect,
}

/// Controls pre-release version inclusion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PreReleasePolicy {
    Allow,
    #[default]
    Disallow,
    IfNecessary,
}

/// Lock file data for warm-start seeding.
pub struct WarmStartData {
    pub packages: Vec<WarmStartPackage>,
}

pub struct WarmStartPackage {
    pub name: String,
    pub version: VypVersion,
    pub dependencies: Vec<(String, VS)>,
}

/// Progress events emitted during resolution.
#[derive(Debug, Clone)]
pub enum ResolveProgress {
    /// A package version was selected for resolution.
    Selecting { package: String, version: String },
    /// Fetching metadata for a package version.
    Fetching { package: String, version: String },
    /// Resolution is complete.
    Complete { package_count: usize },
}

/// Called on the solver thread each time a release is selected, with the
/// wheel that would be installed for it. Must not block: use it to start
/// background work that the caller collects after resolution.
pub type SelectionHook = Box<dyn Fn(&str, &VypVersion, &vyp_api::WheelDist) + Send>;

/// Builder for configuring and running the resolver.
pub struct ResolverBuilder {
    root_dependencies: Vec<Requirement>,
    overrides: Vec<DependencyOverride>,
    errata: Vec<Erratum>,
    substitutions: Vec<SubstitutionSet>,
    plugin_loader: PluginLoader,
    resolution_strategy: ResolutionStrategy,
    pre_release_policy: PreReleasePolicy,
    pre_release_packages: std::collections::HashSet<String>,
    warm_start: Option<WarmStartData>,
    progress: Option<Box<dyn Fn(ResolveProgress) + Send>>,
    on_select: Option<SelectionHook>,
    index_router: Arc<IndexRouter>,
}

impl ResolverBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            root_dependencies: Vec::new(),
            overrides: Vec::new(),
            errata: Vec::new(),
            substitutions: Vec::new(),
            plugin_loader: PluginLoader::new(),
            resolution_strategy: ResolutionStrategy::default(),
            pre_release_policy: PreReleasePolicy::default(),
            pre_release_packages: Default::default(),
            warm_start: None,
            progress: None,
            on_select: None,
            index_router: IndexRouter::new(),
        }
    }

    /// Set the index router that scopes named/explicit indexes to the
    /// transitive closure of their declared root packages.
    pub fn with_index_router(mut self, router: Arc<IndexRouter>) -> Self {
        self.index_router = router;
        self
    }

    /// Add a root dependency requirement.
    pub fn add_dependency(mut self, req: Requirement) -> Self {
        self.root_dependencies.push(req);
        self
    }

    /// Add multiple root dependencies.
    pub fn add_dependencies(mut self, reqs: Vec<Requirement>) -> Self {
        self.root_dependencies.extend(reqs);
        self
    }

    /// Set dependency overrides (unified: range constraints and exact pins).
    pub fn with_overrides(mut self, overrides: Vec<DependencyOverride>) -> Self {
        self.overrides = overrides;
        self
    }

    /// Set metadata errata: corrections to already-published releases.
    pub fn with_errata(mut self, errata: Vec<Erratum>) -> Self {
        self.errata = errata;
        self
    }

    /// Set the substitution sets.
    pub fn with_substitutions(mut self, substitutions: Vec<SubstitutionSet>) -> Self {
        self.substitutions = substitutions;
        self
    }

    /// Set the resolution strategy.
    pub fn with_resolution_strategy(mut self, strategy: ResolutionStrategy) -> Self {
        self.resolution_strategy = strategy;
        self
    }

    /// Set the pre-release policy.
    pub fn with_pre_release_policy(mut self, policy: PreReleasePolicy) -> Self {
        self.pre_release_policy = policy;
        self
    }

    /// Allow pre-releases of these packages regardless of the policy.
    pub fn with_pre_release_packages(mut self, packages: impl IntoIterator<Item = String>) -> Self {
        self.pre_release_packages = packages.into_iter().map(|p| normalize_package_name(&p)).collect();
        self
    }

    /// Set warm-start data from an existing lock file.
    pub fn with_warm_start(mut self, data: WarmStartData) -> Self {
        self.warm_start = Some(data);
        self
    }

    /// Set a progress callback invoked during resolution.
    pub fn with_progress(mut self, cb: impl Fn(ResolveProgress) + Send + 'static) -> Self {
        self.progress = Some(Box::new(cb));
        self
    }

    /// Set a hook invoked whenever the solver selects a release.
    pub fn with_selection_hook(mut self, hook: SelectionHook) -> Self {
        self.on_select = Some(hook);
        self
    }

    /// Access the plugin loader for registering providers and strategies.
    pub fn plugin_loader_mut(&mut self) -> &mut PluginLoader {
        &mut self.plugin_loader
    }

    /// Register a metadata provider directly.
    pub fn with_provider(
        mut self,
        provider: Box<dyn vyp_api::MetadataProvider>,
    ) -> Self {
        self.plugin_loader.providers.register(provider);
        self
    }

    /// Run the resolution.
    ///
    /// The solver runs on a dedicated OS thread so that blocking waits
    /// for metadata (via `Notify`) never interfere with the tokio async
    /// runtime that processes HTTP fetches concurrently.
    pub fn resolve(mut self) -> Result<ResolutionResult, VypError> {
        self.plugin_loader
            .strategies
            .register(Box::new(TransitiveForkStrategy));

        let range_overrides: Vec<DependencyOverride> = self
            .overrides
            .iter()
            .filter(|o| !o.is_exact_pin())
            .cloned()
            .collect();

        let pin_map: HashMap<String, String> = self
            .overrides
            .iter()
            .filter_map(|o| {
                o.pinned_version()
                    .map(|v| (normalize_package_name(&o.package), v.to_string()))
            })
            .collect();

        let override_ranges = override_range_map(self.overrides.iter())?;
        let check_backtracks = self.resolution_strategy == ResolutionStrategy::Highest;
        // Overrides inherited from dependencies rewrite edges between
        // dependencies, never the project's own direct requirements.
        let root_override_ranges =
            override_range_map(self.overrides.iter().filter(|o| o.origin.is_none()))?;
        let applied_overrides = self.overrides.clone();
        let applied_errata = self.errata.clone();

        if !range_overrides.is_empty() {
            self.plugin_loader
                .strategies
                .register(Box::new(OverrideConflictStrategy::new(
                    range_overrides,
                )));
        }

        if !self.substitutions.is_empty() {
            self.plugin_loader.strategies.register(Box::new(
                SubstitutionStrategy::new(self.substitutions.clone()),
            ));
            self.plugin_loader.filters.register(Box::new(
                SubstitutionFilter::new(self.substitutions.clone()),
            ));
        }

        if self.errata.iter().any(|e| e.exclude) {
            self.plugin_loader
                .filters
                .register(Box::new(errata::ErrataFilter::new(self.errata.clone())));
        }

        if self.pre_release_policy != PreReleasePolicy::Allow {
            // PEP 440: a specifier that names a pre-release (`torch>=2.6.0rc1`)
            // asks for that package's pre-releases.
            let mut allowed = self.pre_release_packages.clone();
            for req in &self.root_dependencies {
                if req.constraints.iter().any(|c| c.version.is_pre_release()) {
                    allowed.insert(req.package.name().to_string());
                }
            }
            self.plugin_loader.filters.register(Box::new(
                strategies::pre_release::PreReleaseFilter::with_policy(self.pre_release_policy)
                    .allowing(allowed),
            ));
        }

        let sort_ascending = matches!(
            self.resolution_strategy,
            ResolutionStrategy::Lowest | ResolutionStrategy::LowestDirect
        );

        let root = VypPackage::Root;
        let root_version = VypVersion::new(vec![0]);

        let mut solver = SolverState::new(root.clone(), root_version.clone());
        let mut provenance = ResolutionProvenance::new();
        let mut inherited_conflicts: HashMap<String, ConflictSet> = HashMap::new();

        let mut version_cache: HashMap<String, Arc<Vec<VypVersion>>> = HashMap::new();
        let mut deps_cache: DepsCache = HashMap::new();

        let root_id = solver.root_package;

        let root_dep_ids: Vec<(PackageId, VS)> = self
            .root_dependencies
            .iter()
            .map(|req| {
                let range = root_override_ranges
                    .get(req.package.name())
                    .cloned()
                    .unwrap_or_else(|| requirements_to_range(&req.constraints));
                let pkg_id = solver.get_or_create_package(&req.package);
                (pkg_id, range)
            })
            .collect();

        let root_ranges: HashMap<String, VS> = self
            .root_dependencies
            .iter()
            .zip(&root_dep_ids)
            .map(|(req, (_, range))| (req.package.name().to_string(), range.clone()))
            .collect();

        for (dep_id, dep_range) in &root_dep_ids {
            solver.add_dependency_incompatibility(root_id, &root_version, *dep_id, dep_range);
        }
        solver.mark_deps_added(root_id, &root_version);

        // Fire prefetch for root deps NOW — HTTP requests start immediately
        // on tokio worker threads while we finish setup on this thread.
        {
            let prefetch_names: Vec<String> = self
                .root_dependencies
                .iter()
                .map(|req| req.package.name().to_string())
                .collect();
            if !prefetch_names.is_empty() {
                self.plugin_loader.providers.batch_prefetch(&prefetch_names);
            }
        }

        if let Some(warm) = &self.warm_start {
            for ws_pkg in &warm.packages {
                let pkg = VypPackage::named(&ws_pkg.name);
                let _pkg_id = solver.get_or_create_package(&pkg);

                let warm_deps: Vec<(VypPackage, VS)> = ws_pkg
                    .dependencies
                    .iter()
                    .map(|(dep_name, range)| {
                        let dep = VypPackage::named(dep_name);
                        let _ = solver.get_or_create_package(&dep);
                        (dep.clone(), range.clone())
                    })
                    .collect();

                deps_cache.insert(
                    (ws_pkg.name.clone(), ws_pkg.version.clone()),
                    Arc::new(warm_deps),
                );
            }
        }

        let mut version_counts: HashMap<PackageId, usize> = HashMap::new();

        let mut next_pkg = root_id;

        // Spawn the solver on a dedicated OS thread. This thread blocks
        // on InMemoryIndex::wait_versions/wait_metadata (via Notify) while
        // tokio workers concurrently fetch data over HTTP. The solver
        // thread never touches the tokio runtime, avoiding contention.
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name("vyp-solver".into())
            .spawn(move || {
                let result = Self::run_solver(
                    &mut solver,
                    &mut provenance,
                    &mut inherited_conflicts,
                    &mut version_cache,
                    &mut deps_cache,
                    &mut version_counts,
                    &mut next_pkg,
                    root_id,
                    &root_version,
                    &pin_map,
                    &override_ranges,
                    &self.errata,
                    sort_ascending,
                    &self.plugin_loader,
                    &self.progress,
                    &self.on_select,
                    &self.index_router,
                    check_backtracks,
                    &root_ranges,
                );
                let _ = result_tx.send(result);
            })
            .expect("failed to spawn solver thread");

        let mut result = result_rx.recv().expect("solver thread panicked")?;
        result.overrides = applied_overrides;
        result.errata = applied_errata;
        Ok(result)
    }

    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn run_solver(
        solver: &mut SolverState,
        provenance: &mut ResolutionProvenance,
        inherited_conflicts: &mut HashMap<String, ConflictSet>,
        version_cache: &mut HashMap<String, Arc<Vec<VypVersion>>>,
        deps_cache: &mut DepsCache,
        version_counts: &mut HashMap<PackageId, usize>,
        next_pkg: &mut PackageId,
        _root_id: PackageId,
        _root_version: &VypVersion,
        pin_map: &HashMap<String, String>,
        override_ranges: &OverrideRanges,
        errata: &[Erratum],
        sort_ascending: bool,
        plugin_loader: &PluginLoader,
        progress: &Option<Box<dyn Fn(ResolveProgress) + Send>>,
        on_select: &Option<SelectionHook>,
        index_router: &IndexRouter,
        check_backtracks: bool,
        root_ranges: &HashMap<String, VS>,
    ) -> Result<ResolutionResult, VypError> {
        let profiling = std::env::var("VYP_PROFILE").is_ok_and(|v| v == "1");
        let solve_start = Instant::now();
        let mut version_wait_ns: u128 = 0;
        let mut metadata_wait_ns: u128 = 0;
        let mut version_fetch_count: usize = 0;
        let mut metadata_fetch_count: usize = 0;
        let mut iterations: usize = 0;

        let mut undecided_buf: Vec<(PackageId, VS)> = Vec::new();
        loop {
            iterations += 1;

            // 1. Unit propagation
            let _satisfier_causes = match solver.unit_propagation(*next_pkg) {
                Ok(causes) => causes,
                Err(SolverError::NoSolution { derivation_tree, contested_packages }) => {
                    let mut msg = derivation_tree;

                    if let Some(contested_name) = contested_packages.first() {
                        let contested_pkg = VypPackage::named(contested_name);
                        let requirements: Vec<(String, String)> = contested_packages
                            .iter()
                            .map(|name| (name.clone(), String::new()))
                            .collect();

                        let ctx = ConflictContext {
                            contested_package: contested_pkg,
                            requirements,
                            inherited_conflicts: Vec::new(),
                            current_resolution: solver.partial_solution.extract_solution()
                                .into_iter()
                                .filter_map(|(id, v)| {
                                    let pkg = solver.package_name(id);
                                    if pkg.is_root() { None }
                                    else { Some((pkg.name().to_string(), v)) }
                                })
                                .collect(),
                        };

                        let explanation = resolver::explain::explain_failure(
                            &ctx,
                            &plugin_loader.strategies,
                        );
                        msg.push_str("\n\n");
                        msg.push_str(&explanation.to_string());
                    }

                    return Err(VypError::NoSolution(msg));
                }
                Err(SolverError::Cancelled) => {
                    return Err(VypError::NoSolution("resolution cancelled".to_string()));
                }
            };

            // 2. Pre-visit: fire prefetch for ALL undecided packages
            solver.partial_solution.fill_undecided(&mut undecided_buf);
            let undecided = &undecided_buf;
            {
                let prefetch_names: Vec<String> = undecided
                    .iter()
                    .filter_map(|(pkg_id, _)| {
                        let name = solver.package_name(*pkg_id);
                        if name.is_root() {
                            return None;
                        }
                        let key = name.name().to_string();
                        if !version_cache.contains_key(&key) {
                            Some(key)
                        } else {
                            None
                        }
                    })
                    .collect();

                if !prefetch_names.is_empty() {
                    plugin_loader.providers.batch_prefetch(&prefetch_names);
                }
            }

            // 2b. Opportunistic: populate version_counts for packages
            // whose data has already arrived from prefetch (non-blocking).
            for (pkg_id, _) in undecided.iter() {
                if version_counts.contains_key(pkg_id) {
                    continue;
                }
                let pkg = solver.package_name(*pkg_id);
                if pkg.is_root() {
                    continue;
                }
                if let Some(pvs) = plugin_loader.providers.try_available_versions(pkg) {
                    version_counts.insert(*pkg_id, pvs.versions.len());
                }
            }

            // 3. Pick next package
            let Some((pkg_id, range)) = solver.pick_next_package(version_counts) else {
                let raw_solution = solver.partial_solution.extract_solution();
                let mut packages = HashMap::new();
                for (id, version) in &raw_solution {
                    let pkg = solver.package_name(*id);
                    if !pkg.is_root() {
                        packages.insert(pkg.name().to_string(), version.clone());
                    }
                }

                if let Some(ref cb) = progress {
                    cb(ResolveProgress::Complete { package_count: packages.len() });
                }

                let provenance = resolver::provenance::annotate_provenance(&packages, provenance);
                let backtracks = if check_backtracks {
                    find_backtracks(&packages, root_ranges, version_cache, deps_cache, &plugin_loader.providers)
                } else {
                    Vec::new()
                };

                let wheel_url_start = Instant::now();
                let mut wheel_urls = HashMap::new();
                let mut sdist_urls = HashMap::new();
                for (name, version) in &packages {
                    if let Some(dist) = plugin_loader.providers.wheel_dist(name, version) {
                        wheel_urls.insert(name.clone(), WheelUrlInfo {
                            filename: dist.filename,
                            url: dist.url,
                            hashes: dist.hashes.into_iter().collect(),
                        });
                    } else if let Some(dist) = plugin_loader.providers.sdist_dist(name, version) {
                        sdist_urls.insert(name.clone(), WheelUrlInfo {
                            filename: dist.filename,
                            url: dist.url,
                            hashes: dist.hashes.into_iter().collect(),
                        });
                    }
                }
                let wheel_url_ns = wheel_url_start.elapsed().as_nanos();

                let total_ns = solve_start.elapsed().as_nanos();
                let timing = if profiling {
                    let total_ms = total_ns as f64 / 1_000_000.0;
                    let vw_ms = version_wait_ns as f64 / 1_000_000.0;
                    let mw_ms = metadata_wait_ns as f64 / 1_000_000.0;
                    let wu_ms = wheel_url_ns as f64 / 1_000_000.0;
                    let provider_counters = plugin_loader.providers.collect_profile_data();
                    Some(ResolveTiming {
                        total_ms,
                        version_wait_ms: vw_ms,
                        metadata_wait_ms: mw_ms,
                        solver_ms: total_ms - vw_ms - mw_ms - wu_ms,
                        wheel_url_ms: wu_ms,
                        iterations,
                        version_fetches: version_fetch_count,
                        metadata_fetches: metadata_fetch_count,
                        provider_counters,
                    })
                } else {
                    None
                };

                return Ok(ResolutionResult {
                    packages,
                    provenance,
                    inherited_conflicts: std::mem::take(inherited_conflicts),
                    wheel_urls,
                    sdist_urls,
                    timing,
                    backtracks,
                    source_refs: HashMap::new(),
                    overrides: Vec::new(),
                    errata: Vec::new(),
                    errata_feeds: Vec::new(),
                    exclude_newer: None,
                });
            };
            *next_pkg = pkg_id;

            let pkg = solver.package_name(pkg_id).clone();

            // 4. Choose version
            let versions = get_versions(
                &pkg,
                &plugin_loader.providers,
                &plugin_loader.filters,
                sort_ascending,
                pin_map,
                version_cache,
                provenance,
                &mut version_wait_ns,
                &mut version_fetch_count,
            );

            let mut in_range_count = 0usize;
            let mut chosen: Option<VypVersion> = None;
            for v in versions.iter() {
                if range.contains(v) {
                    in_range_count += 1;
                    if chosen.is_none() {
                        chosen = Some(v.clone());
                    }
                }
            }
            version_counts.insert(pkg_id, in_range_count);

            let Some(version) = chosen else {
                solver.add_incompatibility_for_no_versions(pkg_id, range);
                continue;
            };

            if let Some(ref cb) = progress {
                cb(ResolveProgress::Selecting {
                    package: pkg.name().to_string(),
                    version: version.to_string(),
                });
            }

            provenance.record_selection(
                pkg.name(),
                &version.to_string(),
                SelectionReason::Normal,
                Vec::new(),
            );

            if let Some(hook) = on_select {
                if let Some(dist) = plugin_loader.providers.wheel_dist(pkg.name(), &version) {
                    hook(pkg.name(), &version, &dist);
                }
            }

            // 5. Get dependencies
            let deps = get_dependencies(
                &pkg,
                &version,
                &plugin_loader.providers,
                &plugin_loader.strategies,
                &plugin_loader.filters,
                deps_cache,
                provenance,
                inherited_conflicts,
                pin_map,
                override_ranges,
                errata,
                sort_ascending,
                version_cache,
                &mut metadata_wait_ns,
                &mut metadata_fetch_count,
                index_router,
            );

            let dep_ids: Vec<(PackageId, VS)> = deps
                .iter()
                .map(|(dep_pkg, dep_range)| {
                    let dep_id = solver.get_or_create_package(dep_pkg);
                    (dep_id, dep_range.clone())
                })
                .collect();

            solver.add_dependencies(pkg_id, &version, &dep_ids);
        }
    }
}

impl Default for ResolverBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helper functions for fetching versions and dependencies
// ---------------------------------------------------------------------------

/// Find packages that fell back to a release older than their newest viable
/// one only because the newer releases cap a dependency below the version
/// selected (see [`BacktrackWarning`]).
fn find_backtracks(
    packages: &HashMap<String, VypVersion>,
    root_ranges: &HashMap<String, VS>,
    version_cache: &HashMap<String, Arc<Vec<VypVersion>>>,
    deps_cache: &DepsCache,
    providers: &ProviderRegistry,
) -> Vec<BacktrackWarning> {
    let mut warnings = Vec::new();
    let mut names: Vec<&String> = packages.keys().collect();
    names.sort();
    for name in names {
        let selected = &packages[name];
        let Some(versions) = version_cache.get(name) else { continue };
        let Some(newest) = versions.iter().max() else { continue };
        if newest <= selected {
            continue;
        }
        // If the user or another package rules the newest release out, the
        // fallback is deliberate, not a symptom.
        let mut incoming = root_ranges.get(name).cloned().unwrap_or_else(Ranges::full);
        for (dependent, version) in packages {
            if let Some(deps) = deps_cache.get(&(dependent.clone(), version.clone())) {
                for (pkg, range) in deps.iter() {
                    if pkg.name() == name {
                        incoming = incoming.intersection(range);
                    }
                }
            }
        }
        if !incoming.contains(newest) {
            continue;
        }

        let package = VypPackage::named(name);
        let Some(metadata) = providers
            .providers()
            .iter()
            .filter(|p| p.can_provide(&package))
            .find_map(|p| p.get_metadata(&package, newest).ok().flatten())
        else {
            continue;
        };
        let selected_deps = deps_cache.get(&(name.clone(), selected.clone()));
        for req in &metadata.dependencies {
            let dep = req.package.name();
            let Some(dep_version) = packages.get(dep) else { continue };
            let newest_range = requirements_to_range(&req.constraints);
            // Only an upper bound below the selected version is suspicious;
            // a raised lower bound means the old release is the right one.
            let capped = !newest_range.contains(dep_version)
                && newest_range.intersection(&Ranges::higher_than(dep_version.clone())).is_empty();
            if !capped {
                continue;
            }
            let old_range = selected_deps
                .and_then(|deps| deps.iter().find(|(p, _)| p.name() == dep).map(|(_, r)| r.clone()));
            // The release we fell back to bounding the dependency itself means
            // its maintainers made a call; only an unbounded one is suspect.
            let far_future = VypVersion::new(vec![u32::MAX]);
            if old_range.is_some_and(|r| !r.contains(&far_future)) {
                continue;
            }
            let skipped = versions.iter().filter(|v| *v > selected && *v < newest).count();
            warnings.push(BacktrackWarning {
                package: name.clone(),
                selected: selected.clone(),
                newest: newest.clone(),
                skipped,
                dependency: dep.to_string(),
                dependency_version: dep_version.clone(),
                newest_requires: req
                    .constraints
                    .iter()
                    .map(|c| format!("{}{}", c.op, c.version))
                    .collect::<Vec<_>>()
                    .join(","),
            });
            break;
        }
    }
    warnings
}

/// Parse overrides into the ranges that replace every requirement on each
/// overridden package. Several overrides on one package intersect.
fn override_range_map<'a>(
    overrides: impl Iterator<Item = &'a DependencyOverride>,
) -> Result<OverrideRanges, VypError> {
    let mut map: OverrideRanges = HashMap::new();
    for o in overrides {
        let spec = format!("{} {}", o.package, o.constraint);
        let req: Requirement = spec.parse().map_err(|_| {
            VypError::ResolutionError(format!(
                "invalid override constraint for {}: '{}'",
                o.package, o.constraint
            ))
        })?;
        let range = requirements_to_range(&req.constraints);
        map.entry(req.package.name().to_string())
            .and_modify(|r| *r = r.intersection(&range))
            .or_insert(range);
    }
    Ok(map)
}

#[allow(clippy::too_many_arguments)]
fn get_versions(
    package: &VypPackage,
    provider_registry: &ProviderRegistry,
    filter_registry: &FilterRegistry,
    sort_ascending: bool,
    pin_overrides: &HashMap<String, String>,
    version_cache: &mut HashMap<String, Arc<Vec<VypVersion>>>,
    provenance: &mut ResolutionProvenance,
    version_wait_ns: &mut u128,
    version_fetch_count: &mut usize,
) -> Arc<Vec<VypVersion>> {
    let key = package.name().to_string();
    if let Some(cached) = version_cache.get(&key) {
        return Arc::clone(cached);
    }

    if let Some(pinned_str) = pin_overrides.get(&key) {
        if let Ok(pinned_version) = pinned_str.parse::<VypVersion>() {
            provenance.record_selection(
                &key,
                pinned_str,
                SelectionReason::Override,
                vec![format!("pinned to {} by version pin override", pinned_str)],
            );
            let versions = Arc::new(vec![pinned_version]);
            version_cache.insert(key, Arc::clone(&versions));
            return versions;
        }
    }

    let mut versions = Vec::new();
    for provider in provider_registry.providers() {
        if provider.can_provide(package) {
            let t0 = Instant::now();
            let result = provider.available_versions(package);
            *version_wait_ns += t0.elapsed().as_nanos();
            *version_fetch_count += 1;
            match result {
                Ok(Some(pkg_versions)) => {
                    versions = pkg_versions.versions;
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        "Provider '{}' failed for {}: {}",
                        provider.name(),
                        package.name(),
                        e
                    );
                }
            }
        }
    }

    if sort_ascending {
        versions.sort();
    } else {
        versions.sort_by(|a, b| b.cmp(a));
    }

    if !versions.is_empty() {
        let mut candidates: Vec<Candidate> = versions
            .iter()
            .map(|v| Candidate::new(package.clone(), v.clone()))
            .collect();

        for filter in filter_registry.filters() {
            filter.filter(&mut candidates);
        }

        versions = candidates
            .into_iter()
            .filter(|c| !c.excluded)
            .map(|c| c.version)
            .collect();
    }

    let versions = Arc::new(versions);
    version_cache.insert(key, Arc::clone(&versions));
    versions
}

#[allow(clippy::too_many_arguments)]
fn get_dependencies(
    package: &VypPackage,
    version: &VypVersion,
    provider_registry: &ProviderRegistry,
    strategy_registry: &StrategyRegistry,
    _filter_registry: &FilterRegistry,
    deps_cache: &mut DepsCache,
    provenance: &mut ResolutionProvenance,
    inherited_conflicts: &mut HashMap<String, ConflictSet>,
    _pin_overrides: &HashMap<String, String>,
    override_ranges: &OverrideRanges,
    errata: &[Erratum],
    _sort_ascending: bool,
    version_cache: &mut HashMap<String, Arc<Vec<VypVersion>>>,
    metadata_wait_ns: &mut u128,
    metadata_fetch_count: &mut usize,
    index_router: &IndexRouter,
) -> Arc<Vec<(VypPackage, VS)>> {
    let key = (package.name().to_string(), version.clone());
    if let Some(cached) = deps_cache.get(&key) {
        // Re-propagate index membership even on a cache hit: the parent's
        // routing may have been decided after these deps were first computed.
        let dep_names: Vec<String> =
            cached.iter().map(|(p, _)| p.name().to_string()).collect();
        index_router.propagate(package.name(), &dep_names);
        return Arc::clone(cached);
    }

    let mut deps = Vec::new();
    for provider in provider_registry.providers() {
        if provider.can_provide(package) {
            let t0 = Instant::now();
            let meta_result = provider.get_metadata(package, version);
            *metadata_wait_ns += t0.elapsed().as_nanos();
            *metadata_fetch_count += 1;
            if let Err(ref e) = meta_result {
                tracing::warn!(
                    "Provider '{}' metadata error for {}=={}: {}",
                    provider.name(),
                    package.name(),
                    version,
                    e
                );
            }
            if let Ok(Some(metadata)) = meta_result {
                if !metadata.conflict_declarations.declarations.is_empty() {
                    let pkg_conflicts = inherited_conflicts
                        .entry(package.name().to_string())
                        .or_default();
                    pkg_conflicts.merge(&metadata.conflict_declarations);
                }

                for req in &metadata.dependencies {
                    let range = requirements_to_range(&req.constraints);
                    deps.push((req.package.clone(), range));

                    let dep_record = provenance.records
                        .entry(req.package.name().to_string())
                        .or_default();
                    let requester = format!("{}=={}", package.name(), version);
                    if !dep_record.requested_by.contains(&requester) {
                        dep_record.requested_by.push(requester);
                    }
                }
                break;
            }
        }
    }

    // Errata add requirements the release should have declared. An erratum
    // on a package that is already a dependency narrows its range.
    for erratum in errata.iter().filter(|e| e.applies_to(package.name(), version)) {
        let requester = format!("{}=={} (erratum)", package.name(), version);
        for req in &erratum.requires {
            let range = requirements_to_range(&req.constraints);
            if let Some((_, existing)) = deps.iter_mut().find(|(p, _)| p == &req.package) {
                *existing = existing.intersection(&range);
            } else {
                deps.push((req.package.clone(), range));
            }
            let dep_record = provenance
                .records
                .entry(req.package.name().to_string())
                .or_default();
            if !dep_record.requested_by.contains(&requester) {
                dep_record.requested_by.push(requester.clone());
            }
        }
    }

    // Prefetch version lists for newly discovered deps
    let new_packages: Vec<String> = deps
        .iter()
        .map(|(pkg, _): &(VypPackage, VS)| pkg.name().to_string())
        .filter(|name| !version_cache.contains_key(name))
        .collect();
    if !new_packages.is_empty() {
        provider_registry.batch_prefetch(&new_packages);
    }

    // Consult conflict strategies
    let all_conflicts: Vec<_> = inherited_conflicts
        .values()
        .flat_map(|cs| cs.transitive_declarations().into_iter().cloned())
        .collect();

    if !all_conflicts.is_empty() {
        let context = ConflictContext {
            contested_package: package.clone(),
            requirements: deps
                .iter()
                .map(|(p, r): &(VypPackage, VS)| (p.name().to_string(), format!("{:?}", r)))
                .collect(),
            inherited_conflicts: all_conflicts,
            current_resolution: HashMap::new(),
        };

        for strategy in strategy_registry.strategies() {
            match strategy.evaluate(&context) {
                StrategyVerdict::Abstain => continue,
                StrategyVerdict::RewriteRanges { rewrites } => {
                    let strategy_name = strategy.name().to_string();
                    let reason = strategy.selection_reason();

                    for rewrite in &rewrites {
                        provenance.record_selection(
                            &rewrite.package,
                            "",
                            reason.clone(),
                            vec![format!("rewritten by strategy '{}'", strategy_name)],
                        );

                        for (pkg, range) in &mut deps {
                            if pkg.name() == rewrite.package {
                                let mut new_range = Ranges::full();
                                if let Some(lower) = &rewrite.new_lower {
                                    new_range =
                                        new_range.intersection(&Ranges::higher_than(lower.clone()));
                                }
                                if let Some(upper) = &rewrite.new_upper {
                                    if rewrite.upper_inclusive {
                                        new_range = new_range
                                            .intersection(&Ranges::strictly_lower_than(upper.bump()));
                                    } else {
                                        new_range = new_range
                                            .intersection(&Ranges::strictly_lower_than(upper.clone()));
                                    }
                                }
                                *range = range.intersection(&new_range);
                            }
                        }
                    }
                    break;
                }
                StrategyVerdict::Fork { conflict_name, forks } => {
                    if let Some(fork) = forks.first() {
                        provenance.record_selection(
                            context.contested_package.name(),
                            "",
                            SelectionReason::ConflictResolution,
                            vec![format!(
                                "fork '{}' side '{}' selected",
                                conflict_name, fork.side
                            )],
                        );

                        for (pkg, range) in &mut deps {
                            if pkg.name() == context.contested_package.name() {
                                let mut new_range = Ranges::full();
                                if let Some(lower) = &fork.lower {
                                    new_range = new_range
                                        .intersection(&Ranges::higher_than(lower.clone()));
                                }
                                if let Some(upper) = &fork.upper {
                                    if fork.upper_inclusive {
                                        new_range = new_range.intersection(
                                            &Ranges::strictly_lower_than(upper.bump()),
                                        );
                                    } else {
                                        new_range = new_range.intersection(
                                            &Ranges::strictly_lower_than(upper.clone()),
                                        );
                                    }
                                }
                                *range = range.intersection(&new_range);
                            }
                        }
                    }
                    break;
                }
                StrategyVerdict::Fail { .. } => break,
            }
        }
    }

    // Overrides are the final word: they replace whatever range this release
    // (or an erratum, or a strategy) asked for.
    for (pkg, range) in &mut deps {
        if let Some(forced) = override_ranges.get(pkg.name()) {
            *range = forced.clone();
        }
    }

    // Grow index scopes along this package's dependency edges so that named
    // indexes route the transitive closure of their declared roots.
    let dep_names: Vec<String> = deps.iter().map(|(p, _)| p.name().to_string()).collect();
    index_router.propagate(package.name(), &dep_names);

    let deps = Arc::new(deps);
    deps_cache.insert(key, Arc::clone(&deps));
    deps
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use vyp_index::OfflineMetadataProvider;
    use vyp_api::{ComparisonOp, Requirement};

    fn setup_simple_graph() -> OfflineMetadataProvider {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("icons", VypVersion::from_parts(1, 0, 0), vec![]);
        provider.add_package("icons", VypVersion::from_parts(2, 0, 0), vec![]);

        provider.add_package(
            "dropdown",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("icons").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        provider.add_package(
            "menu",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("dropdown").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        provider
    }

    #[test]
    fn test_basic_resolution() {
        let offline = setup_simple_graph();
        let result = ResolverBuilder::new()
            .with_provider(Box::new(offline))
            .add_dependency(
                Requirement::new("menu").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("icons").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();

        assert!(result.packages.contains_key("menu"));
        assert!(result.packages.contains_key("icons"));
        assert!(result.packages.contains_key("dropdown"));
        assert_eq!(
            result.packages.get("icons").unwrap(),
            &VypVersion::from_parts(2, 0, 0)
        );
    }

    #[test]
    fn test_conflict_no_solution() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("numpy", VypVersion::from_parts(1, 26, 0), vec![]);
        provider.add_package("numpy", VypVersion::from_parts(2, 0, 0), vec![]);

        provider.add_package(
            "pkg-a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("numpy").with_constraint(
                ComparisonOp::Lt,
                VypVersion::from_parts(2, 0, 0),
            )],
        );

        provider.add_package(
            "pkg-b",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("numpy").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(2, 0, 0),
            )],
        );

        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .add_dependency(
                Requirement::new("pkg-a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("pkg-b").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve();

        assert!(result.is_err());
        if let Err(VypError::NoSolution(msg)) = result {
            assert!(msg.contains("numpy"));
        }
    }

    #[test]
    fn test_diamond_resolution() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("c", VypVersion::from_parts(1, 0, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(1, 5, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(2, 0, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(3, 0, 0), vec![]);

        provider.add_package(
            "a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("c")
                .with_constraint(ComparisonOp::Gte, VypVersion::from_parts(1, 0, 0))
                .with_constraint(ComparisonOp::Lt, VypVersion::from_parts(2, 0, 0))],
        );

        provider.add_package(
            "b",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("c")
                .with_constraint(ComparisonOp::Gte, VypVersion::from_parts(1, 5, 0))
                .with_constraint(ComparisonOp::Lt, VypVersion::from_parts(3, 0, 0))],
        );

        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .add_dependency(
                Requirement::new("a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("b").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();

        let c_version = result.packages.get("c").unwrap();
        assert_eq!(c_version, &VypVersion::from_parts(1, 5, 0));
    }

    #[test]
    fn test_provenance_recorded() {
        let offline = setup_simple_graph();
        let result = ResolverBuilder::new()
            .with_provider(Box::new(offline))
            .add_dependency(
                Requirement::new("menu").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();

        assert!(result.provenance.records.contains_key("menu"));
        assert!(result.provenance.records.contains_key("dropdown"));
        assert!(result.provenance.records.contains_key("icons"));
    }

    #[test]
    fn test_transitive_conflict_declarations_propagate() {
        use vyp_api::{ConflictDeclaration, ConflictSet};

        let mut provider = OfflineMetadataProvider::new();

        let mut pkg_a_conflicts = ConflictSet::new();
        pkg_a_conflicts.add(
            ConflictDeclaration::new(
                "numpy-device",
                vec!["gpu".into(), "cpu".into()],
                vec!["numpy".into()],
            )
            .with_transitive(true),
        );

        provider.add_package_with_conflicts(
            "pkg-a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("numpy").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
            pkg_a_conflicts,
        );

        provider.add_package(
            "pkg-b",
            VypVersion::from_parts(1, 0, 0),
            vec![
                Requirement::new("pkg-a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
                Requirement::new("numpy").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            ],
        );

        provider.add_package("numpy", VypVersion::from_parts(1, 26, 0), vec![]);
        provider.add_package("numpy", VypVersion::from_parts(2, 0, 0), vec![]);

        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .add_dependency(
                Requirement::new("pkg-b").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();

        assert!(result.packages.contains_key("pkg_a"));
        assert!(result.packages.contains_key("pkg_b"));
        assert!(result.packages.contains_key("numpy"));

        let has_inherited = result
            .inherited_conflicts
            .values()
            .any(|cs| {
                cs.declarations
                    .iter()
                    .any(|d| d.name == "numpy-device" && d.transitive)
            });
        assert!(has_inherited, "Transitive conflict should be inherited");
    }

    #[test]
    fn test_range_override_resolves_conflict() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("numpy", VypVersion::from_parts(1, 26, 0), vec![]);
        provider.add_package("numpy", VypVersion::from_parts(2, 0, 0), vec![]);

        provider.add_package(
            "pkg-a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("numpy").with_constraint(
                ComparisonOp::Lt,
                VypVersion::from_parts(2, 0, 0),
            )],
        );

        provider.add_package(
            "pkg-b",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("numpy").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        let overrides = vec![
            DependencyOverride::new("numpy", ">=1.26,<2"),
        ];

        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .with_overrides(overrides)
            .add_dependency(
                Requirement::new("pkg-a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("pkg-b").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();

        assert_eq!(
            result.packages.get("numpy").unwrap(),
            &VypVersion::from_parts(1, 26, 0)
        );
    }

    #[test]
    fn test_substitution_prefers_configured() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("opencv_python", VypVersion::from_parts(4, 9, 0), vec![]);
        provider.add_package("opencv_python_headless", VypVersion::from_parts(4, 9, 0), vec![]);

        let substitutions = vec![SubstitutionSet::new(
            "opencv",
            vec!["opencv_python".into(), "opencv_python_headless".into()],
        )
        .with_preference("opencv_python_headless")];

        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .with_substitutions(substitutions)
            .add_dependency(
                Requirement::new("opencv_python_headless").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(4, 0, 0),
                ),
            )
            .resolve()
            .unwrap();

        assert!(result.packages.contains_key("opencv_python_headless"));
    }

    // -----------------------------------------------------------------------
    // Backtracking tests: verify that the solver backtracks when needed
    // and measure iteration counts. These are critical for understanding
    // the risk of streaming downloads mid-resolve.
    // -----------------------------------------------------------------------

    /// Simple version downgrade: solver picks A 2.0 which requires C>=2.0,
    /// but B requires C<2.0. Solver must backtrack to A 1.0.
    #[test]
    fn test_backtrack_simple_version_downgrade() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("c", VypVersion::from_parts(1, 0, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(1, 5, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(2, 0, 0), vec![]);

        provider.add_package(
            "a",
            VypVersion::from_parts(2, 0, 0),
            vec![Requirement::new("c").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(2, 0, 0),
            )],
        );
        provider.add_package(
            "a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("c").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        provider.add_package(
            "b",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("c").with_constraint(
                ComparisonOp::Lt,
                VypVersion::from_parts(2, 0, 0),
            )],
        );

        std::env::set_var("VYP_PROFILE", "1");
        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .add_dependency(
                Requirement::new("a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("b").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();
        std::env::remove_var("VYP_PROFILE");

        assert_eq!(
            result.packages.get("a").unwrap(),
            &VypVersion::from_parts(1, 0, 0),
            "solver should backtrack from A 2.0 to A 1.0"
        );
        assert_eq!(
            result.packages.get("c").unwrap(),
            &VypVersion::from_parts(1, 5, 0),
            "C should be highest compatible: 1.5.0"
        );

        let timing = result.timing.as_ref().unwrap();
        let pkg_count = result.packages.len();
        eprintln!(
            "backtrack_simple: {} packages, {} iterations (overhead: {})",
            pkg_count,
            timing.iterations,
            timing.iterations as isize - pkg_count as isize,
        );
        assert!(
            timing.iterations > pkg_count,
            "should require more iterations than packages (backtracking), got {} iterations for {} pkgs",
            timing.iterations,
            pkg_count,
        );
    }

    /// Chain backtrack: A 2.0 → B>=2.0 → C>=3.0, but D requires C<3.0.
    /// Solver must backtrack through A→B chain.
    #[test]
    fn test_backtrack_chain() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("c", VypVersion::from_parts(1, 0, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(2, 0, 0), vec![]);
        provider.add_package("c", VypVersion::from_parts(3, 0, 0), vec![]);

        provider.add_package(
            "b",
            VypVersion::from_parts(2, 0, 0),
            vec![Requirement::new("c").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(3, 0, 0),
            )],
        );
        provider.add_package(
            "b",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("c").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        provider.add_package(
            "a",
            VypVersion::from_parts(2, 0, 0),
            vec![Requirement::new("b").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(2, 0, 0),
            )],
        );
        provider.add_package(
            "a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("b").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        provider.add_package(
            "d",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("c").with_constraint(
                ComparisonOp::Lt,
                VypVersion::from_parts(3, 0, 0),
            )],
        );

        std::env::set_var("VYP_PROFILE", "1");
        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .add_dependency(
                Requirement::new("a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("d").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();
        std::env::remove_var("VYP_PROFILE");

        let timing = result.timing.as_ref().unwrap();
        let pkg_count = result.packages.len();
        eprintln!(
            "backtrack_chain: {} packages, {} iterations (overhead: {})",
            pkg_count,
            timing.iterations,
            timing.iterations as isize - pkg_count as isize,
        );
        assert!(
            timing.iterations > pkg_count,
            "chain scenario should backtrack: {} iterations for {} pkgs",
            timing.iterations,
            pkg_count,
        );
        assert_eq!(
            result.packages.get("a").unwrap(),
            &VypVersion::from_parts(1, 0, 0),
        );
    }

    /// Multiple cascading backtracks: solver tries A 3.0, 2.0, finally 1.0.
    #[test]
    fn test_backtrack_multiple_cascading() {
        let mut provider = OfflineMetadataProvider::new();

        provider.add_package("d", VypVersion::from_parts(1, 0, 0), vec![]);
        provider.add_package("d", VypVersion::from_parts(2, 0, 0), vec![]);
        provider.add_package("d", VypVersion::from_parts(3, 0, 0), vec![]);

        provider.add_package(
            "a",
            VypVersion::from_parts(3, 0, 0),
            vec![Requirement::new("d").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(3, 0, 0),
            )],
        );
        provider.add_package(
            "a",
            VypVersion::from_parts(2, 0, 0),
            vec![Requirement::new("d").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(2, 0, 0),
            )],
        );
        provider.add_package(
            "a",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("d").with_constraint(
                ComparisonOp::Gte,
                VypVersion::from_parts(1, 0, 0),
            )],
        );

        provider.add_package(
            "b",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("d").with_constraint(
                ComparisonOp::Lt,
                VypVersion::from_parts(3, 0, 0),
            )],
        );

        provider.add_package(
            "c",
            VypVersion::from_parts(1, 0, 0),
            vec![Requirement::new("d").with_constraint(
                ComparisonOp::Lt,
                VypVersion::from_parts(2, 0, 0),
            )],
        );

        std::env::set_var("VYP_PROFILE", "1");
        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider))
            .add_dependency(
                Requirement::new("a").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("b").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .add_dependency(
                Requirement::new("c").with_constraint(
                    ComparisonOp::Gte,
                    VypVersion::from_parts(1, 0, 0),
                ),
            )
            .resolve()
            .unwrap();
        std::env::remove_var("VYP_PROFILE");

        let timing = result.timing.as_ref().unwrap();
        let pkg_count = result.packages.len();
        eprintln!(
            "backtrack_multi: {} packages, {} iterations (overhead: {})",
            pkg_count,
            timing.iterations,
            timing.iterations as isize - pkg_count as isize,
        );
        assert!(
            timing.iterations > pkg_count + 1,
            "should backtrack at least twice: {} iterations for {} pkgs",
            timing.iterations,
            pkg_count,
        );
        assert_eq!(
            result.packages.get("a").unwrap(),
            &VypVersion::from_parts(1, 0, 0),
            "solver should land on A 1.0 after two backtracks"
        );
        assert_eq!(
            result.packages.get("d").unwrap(),
            &VypVersion::from_parts(1, 0, 0),
        );
    }

    #[test]
    fn test_marker_parsing_filters_extras() {
        use vyp_api::MarkerEnvironment;

        let env = MarkerEnvironment::current();

        let cases = vec![
            ("charset-normalizer (<4,>=2)", true),
            ("idna (<4,>=2.5)", true),
            ("urllib3 (<3,>=1.21.1)", true),
            ("certifi (>=2017.4.17)", true),
            (r#"PySocks (!=1.5.7,>=1.5.6) ; extra == "socks""#, false),
            (r#"chardet (<6,>=3.0.2) ; extra == "use_chardet_on_py3""#, false),
            (r#"brotli (>=1.0.9) ; platform_python_implementation == "CPython" and extra == "brotli""#, false),
            (r#"h2 (<5,>=4) ; extra == "h2""#, false),
        ];

        for (line, expected) in cases {
            let req: Requirement = line.parse().unwrap();
            let result = match &req.marker {
                None => true,
                Some(tree) => tree.evaluate(&env, &[]),
            };
            assert_eq!(result, expected, "Failed for: {}", line);
        }
    }
}

#[cfg(test)]
mod override_and_errata_tests {
    use super::*;
    use vyp_api::{ComparisonOp, Requirement};
    use vyp_index::OfflineMetadataProvider;

    fn v(major: u32, minor: u32) -> VypVersion {
        VypVersion::from_parts(major, minor, 0)
    }

    fn req(name: &str, op: ComparisonOp, version: VypVersion) -> Requirement {
        Requirement::new(name).with_constraint(op, version)
    }

    fn numpy_provider() -> OfflineMetadataProvider {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("numpy", v(1, 24), vec![]);
        p.add_package("numpy", v(1, 26), vec![]);
        p.add_package("numpy", v(2, 0), vec![]);
        p
    }

    #[test]
    fn range_override_applies_without_inherited_conflicts() {
        let mut p = numpy_provider();
        p.add_package("pkg-b", v(1, 0), vec![req("numpy", ComparisonOp::Gte, v(1, 0))]);

        let result = ResolverBuilder::new()
            .with_provider(Box::new(p))
            .with_overrides(vec![DependencyOverride::new("numpy", "<2")])
            .add_dependency(Requirement::new("pkg-b"))
            .resolve()
            .unwrap();

        assert_eq!(result.packages["numpy"], v(1, 26));
    }

    #[test]
    fn override_replaces_conflicting_constraints() {
        // lib-a wants numpy>=1.26 and lib-b wants numpy<1.25: unsolvable
        // unless the override replaces both requirements.
        let mut p = numpy_provider();
        p.add_package("lib-a", v(1, 0), vec![req("numpy", ComparisonOp::Gte, v(1, 26))]);
        p.add_package("lib-b", v(1, 0), vec![req("numpy", ComparisonOp::Lt, v(1, 25))]);

        let result = ResolverBuilder::new()
            .with_provider(Box::new(p))
            .with_overrides(vec![DependencyOverride::new("numpy", ">=1.26,<2")])
            .add_dependency(Requirement::new("lib-a"))
            .add_dependency(Requirement::new("lib-b"))
            .resolve()
            .unwrap();

        assert_eq!(result.packages["numpy"], v(1, 26));
    }

    #[test]
    fn pin_override_matches_unnormalized_names() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("scikit-learn", v(1, 3), vec![]);
        p.add_package("scikit-learn", v(1, 5), vec![]);

        let result = ResolverBuilder::new()
            .with_provider(Box::new(p))
            .with_overrides(vec![DependencyOverride::new("Scikit-Learn", "==1.3.0")])
            .add_dependency(Requirement::new("scikit-learn"))
            .resolve()
            .unwrap();

        assert_eq!(result.packages["scikit_learn"], v(1, 3));
    }

    #[test]
    fn excluding_erratum_skips_broken_releases() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("green", v(3, 9), vec![]);
        p.add_package("green", v(4, 0), vec![]);
        p.add_package("green", v(4, 1), vec![]);

        // 4.1 is broken on this interpreter; 4.0 is fine.
        let mut erratum = Erratum::new("green", Ranges::singleton(v(4, 1)), "test-feed");
        erratum.exclude = true;

        let result = ResolverBuilder::new()
            .with_provider(Box::new(p))
            .with_errata(vec![erratum])
            .add_dependency(Requirement::new("green"))
            .resolve()
            .unwrap();

        assert_eq!(result.packages["green"], v(4, 0));
    }

    #[test]
    fn requiring_erratum_adds_missing_upper_bound() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("libx", v(1, 0), vec![]);
        p.add_package("libx", v(2, 0), vec![]);
        p.add_package("app-lib", v(1, 0), vec![Requirement::new("libx")]);
        p.add_package("other", v(1, 0), vec![]);

        let mut erratum = Erratum::new("app-lib", Ranges::full(), "test-feed");
        erratum.requires = vec![
            req("libx", ComparisonOp::Lt, v(2, 0)),
            Requirement::new("other"),
        ];

        let result = ResolverBuilder::new()
            .with_provider(Box::new(p))
            .with_errata(vec![erratum])
            .add_dependency(Requirement::new("app-lib"))
            .resolve()
            .unwrap();

        assert_eq!(result.packages["libx"], v(1, 0));
        assert!(result.packages.contains_key("other"));
        assert!(result.provenance.records["other"]
            .requested_by
            .iter()
            .any(|r| r.contains("erratum")));
    }
}

#[cfg(test)]
mod inherited_override_tests {
    use super::*;
    use vyp_api::{ComparisonOp, Requirement};
    use vyp_index::OfflineMetadataProvider;

    #[test]
    fn inherited_override_does_not_replace_direct_requirements() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("libx", VypVersion::from_parts(1, 0, 0), vec![]);
        p.add_package("libx", VypVersion::from_parts(2, 0, 0), vec![]);
        p.add_package("lib-a", VypVersion::from_parts(1, 0, 0), vec![
            Requirement::new("libx").with_constraint(ComparisonOp::Lt, VypVersion::from_parts(2, 0, 0)),
        ]);

        let mut inherited = DependencyOverride::new("libx", "<2").inherit_through("lib-a");
        inherited.transitive = true;

        // The project asks for libx>=2 directly; lib-a's inherited override
        // must not silently replace that, so this has no solution.
        let err = ResolverBuilder::new()
            .with_provider(Box::new(p))
            .with_overrides(vec![inherited])
            .add_dependency(Requirement::new("lib-a"))
            .add_dependency(
                Requirement::new("libx").with_constraint(ComparisonOp::Gte, VypVersion::from_parts(2, 0, 0)),
            )
            .resolve();
        assert!(err.is_err());
    }
}

#[cfg(test)]
mod pre_release_tests {
    use super::*;
    use vyp_api::Requirement;
    use vyp_index::OfflineMetadataProvider;

    fn provider() -> OfflineMetadataProvider {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("torch", "2.5.0".parse().unwrap(), vec![Requirement::new("sympy")]);
        p.add_package("torch", "2.6.0rc1".parse().unwrap(), vec![Requirement::new("sympy")]);
        p.add_package("sympy", "1.12.0".parse().unwrap(), vec![]);
        p.add_package("sympy", "1.13.0a1".parse().unwrap(), vec![]);
        p
    }

    #[test]
    fn named_packages_get_pre_releases_and_dependencies_do_not() {
        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider()))
            .with_pre_release_packages(["Torch".to_string()])
            .add_dependency(Requirement::new("torch"))
            .resolve()
            .unwrap();
        assert_eq!(result.packages["torch"].to_string(), "2.6.0rc1");
        assert_eq!(result.packages["sympy"].to_string(), "1.12.0");
    }

    #[test]
    fn pre_release_specifier_opts_that_package_in() {
        let req: Requirement = "torch>=2.6.0rc1".parse().unwrap();
        let result = ResolverBuilder::new()
            .with_provider(Box::new(provider()))
            .add_dependency(req)
            .resolve()
            .unwrap();
        assert_eq!(result.packages["torch"].to_string(), "2.6.0rc1");
        assert_eq!(result.packages["sympy"].to_string(), "1.12.0");
    }
}

#[cfg(test)]
mod backtrack_tests {
    use super::*;
    use vyp_api::{ComparisonOp, Requirement};
    use vyp_index::OfflineMetadataProvider;

    fn v(s: &str) -> VypVersion {
        s.parse().unwrap()
    }

    fn resolve(p: OfflineMetadataProvider, deps: Vec<Requirement>) -> ResolutionResult {
        ResolverBuilder::new().with_provider(Box::new(p)).add_dependencies(deps).resolve().unwrap()
    }

    #[test]
    fn warns_when_newer_releases_cap_a_dependency_the_old_one_ignores() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("libx", v("1.0"), vec![]);
        p.add_package("libx", v("2.0"), vec![]);
        // green 3.x never bounded libx; 4.x learned it breaks on libx 2.
        p.add_package("green", v("3.9"), vec![Requirement::new("libx")]);
        p.add_package("green", v("4.0"), vec![Requirement::new("libx").with_constraint(ComparisonOp::Lt, v("2"))]);
        p.add_package("green", v("4.1"), vec![Requirement::new("libx").with_constraint(ComparisonOp::Lt, v("2"))]);

        let result = resolve(p, vec![
            Requirement::new("green"),
            Requirement::new("libx").with_constraint(ComparisonOp::Gte, v("2")),
        ]);
        assert_eq!(result.packages["green"], v("3.9"));
        assert_eq!(result.backtracks.len(), 1, "{:?}", result.backtracks);
        let w = &result.backtracks[0];
        assert_eq!((w.package.as_str(), w.dependency.as_str(), w.skipped), ("green", "libx", 1));
        assert_eq!(w.newest, v("4.1"));
        assert!(w.describe().contains("libx<2"), "{}", w.describe());
    }

    #[test]
    fn no_warning_when_the_user_rules_out_newer_releases() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("libx", v("2.0"), vec![]);
        p.add_package("green", v("3.9"), vec![Requirement::new("libx")]);
        p.add_package("green", v("4.1"), vec![Requirement::new("libx").with_constraint(ComparisonOp::Lt, v("2"))]);
        let result = resolve(p, vec![
            Requirement::new("green").with_constraint(ComparisonOp::Lt, v("4")),
            Requirement::new("libx"),
        ]);
        assert!(result.backtracks.is_empty(), "{:?}", result.backtracks);
    }

    #[test]
    fn no_warning_when_newer_releases_raise_a_lower_bound() {
        let mut p = OfflineMetadataProvider::new();
        p.add_package("libx", v("1.0"), vec![]);
        p.add_package("libx", v("2.0"), vec![]);
        p.add_package("green", v("3.9"), vec![Requirement::new("libx")]);
        p.add_package("green", v("4.1"), vec![Requirement::new("libx").with_constraint(ComparisonOp::Gte, v("2"))]);
        let result = resolve(p, vec![
            Requirement::new("green"),
            Requirement::new("libx").with_constraint(ComparisonOp::Lt, v("2")),
        ]);
        assert_eq!(result.packages["green"], v("3.9"));
        assert!(result.backtracks.is_empty(), "{:?}", result.backtracks);
    }
}
