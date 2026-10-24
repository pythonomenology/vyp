use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vyp_api::{DependencyOverride, MarkerEnvironment, Requirement, SubstitutionSet};
use vyp_core::IndexRouter;

/// A named package index.
#[derive(Debug, Clone)]
pub struct IndexConfig {
    pub name: String,
    pub url: String,
    /// When true, this index is only used for packages explicitly routed here
    /// via `[tool.vyp.sources]`.
    pub explicit: bool,
}

/// Metadata providers for a resolution, and the router scoping named indexes.
pub type Providers = (Vec<Box<dyn vyp_api::MetadataProvider>>, Arc<IndexRouter>);

/// A per-package source routing entry.
#[derive(Debug, Clone)]
pub struct SourceEntry {
    pub index: String,
    /// Optional PEP 508 environment marker (e.g. `sys_platform == 'linux'`).
    #[allow(dead_code)]
    pub marker: Option<String>,
}

/// Parsed [tool.vyp] configuration from pyproject.toml.
#[derive(Debug, Clone)]
pub struct VypConfig {
    pub resolution_strategy: ResolutionStrategy,
    pub pre_releases: PreReleasePolicy,
    pub dependencies: Vec<String>,
    /// PEP 621: `[project].optional-dependencies`
    pub optional_dependencies: HashMap<String, Vec<String>>,
    /// PEP 735: `[dependency-groups]` -- top-level table of dependency groups
    pub dependency_groups: HashMap<String, Vec<String>>,
    pub overrides: Vec<DependencyOverride>,
    pub substitutions: Vec<SubstitutionSet>,
    /// PEP 621 `requires-python` specifier (e.g. `">=3.10"`).
    pub requires_python: Option<String>,
    /// Primary index URL (defaults to `https://pypi.org/simple`).
    pub index_url: String,
    /// Named extra indexes.
    pub extra_indexes: Vec<IndexConfig>,
    /// Per-package index routing.
    pub sources: HashMap<String, Vec<SourceEntry>>,
    /// Torch backend selection: `auto`, `cpu`, `cu126`, `cu128`, etc.
    pub torch_backend: Option<String>,
    pub plugin_search_paths: Vec<String>,
    pub plugin_loads: Vec<PluginLoadConfig>,
    /// Environment marker strings for universal resolution (e.g. `python_version == "3.8"`).
    /// When empty, single-environment resolution (current behavior).
    pub environments: Vec<String>,
    /// When resolving for multiple environments: requires-python vs fewest.
    pub fork_strategy: ForkStrategy,
    /// Errata feeds (URLs or paths relative to the project directory).
    pub errata: Vec<String>,
    /// Which dependencies may ship overrides that apply to this resolution.
    pub inherit_overrides: InheritOverrides,
    /// Directory containing the pyproject.toml this config was read from.
    pub project_dir: Option<PathBuf>,
    /// Preferred interpreter for the project environment (`python = "3.12"`).
    pub python: Option<String>,
    /// Interpreter to resolve for. Set by commands that know their target
    /// environment; otherwise the project's `.venv` or a matching interpreter.
    pub target_python: Option<PathBuf>,
    /// What to do with packages that ship code run at interpreter startup.
    pub startup_files: crate::cache::startup::StartupPolicy,
    /// Ignore distributions uploaded after a cutoff.
    pub exclude_newer: vyp_index::ExcludeNewer,
    /// When versions that need an sdist build are considered.
    pub sdists: vyp_index::SdistPolicy,
    /// How resolution obtains metadata for sdists without static metadata.
    pub sdist_builder: Option<SdistBuilderHook>,
    /// Packages whose pre-releases are allowed whatever `pre-releases` says.
    pub pre_release_packages: Vec<String>,
    /// Path and git sources, by declared package name.
    pub local_sources: Vec<(String, crate::build::sources::LocalSource)>,
}

#[derive(Clone)]
pub struct SdistBuilderHook(pub vyp_index::sdist::SdistMetadataBuilder);

impl std::fmt::Debug for SdistBuilderHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SdistBuilderHook")
    }
}

/// Parse an `exclude-newer` value: an RFC 3339 timestamp or date, or a
/// duration back from now (`P7D`, `7 days`).
pub fn parse_cutoff(s: &str) -> miette::Result<i64> {
    vyp_index::timestamp::parse_cutoff(s).ok_or_else(|| {
        miette::miette!(
            "Invalid exclude-newer value '{}': use a date (2026-01-31), a timestamp (2026-01-31T12:00:00Z), or a duration (P7D, \"7 days\")",
            s
        )
    })
}

/// `inherit-overrides`: `true`, `false`, or an allowlist of package names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InheritOverrides {
    #[default]
    All,
    None,
    Only(HashSet<String>),
}

impl InheritOverrides {
    pub fn allows(&self, package: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Only(names) => names.contains(&vyp_api::normalize_package_name(package)),
        }
    }
}

impl Default for VypConfig {
    fn default() -> Self {
        Self {
            resolution_strategy: ResolutionStrategy::default(),
            pre_releases: PreReleasePolicy::default(),
            dependencies: Vec::new(),
            optional_dependencies: HashMap::new(),
            dependency_groups: HashMap::new(),
            overrides: Vec::new(),
            substitutions: Vec::new(),
            requires_python: None,
            index_url: "https://pypi.org/simple".to_string(),
            extra_indexes: Vec::new(),
            sources: HashMap::new(),
            torch_backend: None,
            plugin_search_paths: Vec::new(),
            plugin_loads: Vec::new(),
            environments: Vec::new(),
            fork_strategy: ForkStrategy::default(),
            errata: Vec::new(),
            inherit_overrides: InheritOverrides::default(),
            project_dir: None,
            python: None,
            target_python: None,
            startup_files: Default::default(),
            exclude_newer: Default::default(),
            sdists: Default::default(),
            sdist_builder: None,
            pre_release_packages: Vec::new(),
            local_sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub enum ResolutionStrategy {
    #[default]
    Highest,
    Lowest,
    LowestDirect,
}

#[derive(Debug, Clone, Copy, Default)]
pub enum PreReleasePolicy {
    Allow,
    #[default]
    Disallow,
    IfNecessary,
}

/// When resolving for multiple environments, how to choose versions across envs.
#[derive(Debug, Clone, Copy, Default)]
pub enum ForkStrategy {
    /// Prefer latest version per environment (one version per Python band).
    #[default]
    RequiresPython,
    /// Minimize distinct versions; prefer one version for all environments when possible.
    Fewest,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PluginLoadConfig {
    pub name: String,
    pub path: Option<String>,
    pub config: HashMap<String, toml::Value>,
}

// ---------------------------------------------------------------------------
// Raw TOML deserialization structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct RawPyProject {
    #[serde(default)]
    tool: Option<RawTool>,
    #[serde(default)]
    project: Option<RawProject>,
    #[serde(default, rename = "dependency-groups")]
    dependency_groups: HashMap<String, Vec<toml::Value>>,
}

#[derive(Debug, Deserialize, Default)]
struct RawProject {
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default, rename = "optional-dependencies")]
    optional_dependencies: HashMap<String, Vec<String>>,
    #[serde(default, rename = "requires-python")]
    requires_python: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTool {
    #[serde(default)]
    vyp: Option<RawVypConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RawVypConfig {
    #[serde(default, rename = "resolution-strategy")]
    resolution_strategy: Option<String>,
    #[serde(default, rename = "pre-releases")]
    pre_releases: Option<String>,
    #[serde(default, rename = "index-url")]
    index_url: Option<String>,
    #[serde(default, rename = "extra-index")]
    extra_index: Vec<RawExtraIndex>,
    #[serde(default)]
    sources: HashMap<String, Vec<RawSourceEntry>>,
    #[serde(default, rename = "torch-backend")]
    torch_backend: Option<String>,
    #[serde(default)]
    environments: Vec<String>,
    #[serde(default, rename = "fork-strategy")]
    fork_strategy: Option<String>,
    #[serde(default)]
    overrides: Vec<RawOverride>,
    #[serde(default)]
    substitutions: Vec<RawSubstitution>,
    #[serde(default)]
    plugins: Option<RawPlugins>,
    #[serde(default)]
    errata: Vec<String>,
    #[serde(default, rename = "inherit-overrides")]
    inherit_overrides: Option<RawInheritOverrides>,
    #[serde(default)]
    python: Option<String>,
    #[serde(default, rename = "startup-files")]
    startup_files: Option<String>,
    #[serde(default, rename = "allow-startup-files")]
    allow_startup_files: Vec<String>,
    #[serde(default, rename = "exclude-newer")]
    exclude_newer: Option<String>,
    #[serde(default, rename = "exclude-newer-package")]
    exclude_newer_package: HashMap<String, toml::Value>,
    #[serde(default)]
    sdists: Option<String>,
    #[serde(default, rename = "pre-release-packages")]
    pre_release_packages: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawInheritOverrides {
    Enabled(bool),
    Only(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct RawExtraIndex {
    name: String,
    url: String,
    #[serde(default)]
    explicit: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSourceEntry {
    #[serde(default)]
    index: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    editable: bool,
    #[serde(default)]
    git: Option<String>,
    #[serde(default)]
    rev: Option<String>,
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    subdirectory: Option<String>,
    #[serde(default)]
    marker: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawOverride {
    package: String,
    constraint: String,
    #[serde(default)]
    transitive: bool,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSubstitution {
    provides: String,
    packages: Vec<String>,
    #[serde(default)]
    prefer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawPlugins {
    #[serde(default, rename = "search-paths")]
    search_paths: Vec<String>,
    #[serde(default)]
    load: Vec<RawPluginLoad>,
}

#[derive(Debug, Deserialize)]
struct RawPluginLoad {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    config: HashMap<String, toml::Value>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl VypConfig {
    /// Make `path` sources absolute, and collect direct references
    /// (`pkg @ file://...`, `pkg @ git+...`) from the dependency lists.
    fn resolve_local_source_paths(&mut self, project_dir: &Path) {
        use crate::build::sources::LocalSource;
        for (_, source) in &mut self.local_sources {
            if let LocalSource::Path { path, .. } = source {
                if path.is_relative() {
                    *path = project_dir.join(&*path);
                }
            }
        }
        let all_deps = self
            .dependencies
            .iter()
            .chain(self.optional_dependencies.values().flatten())
            .chain(self.dependency_groups.values().flatten());
        let mut direct = Vec::new();
        for dep in all_deps {
            let Ok(req) = dep.parse::<Requirement>() else {
                continue;
            };
            let Some(url) = &req.url else { continue };
            let name = req.package.name().to_string();
            if self
                .local_sources
                .iter()
                .any(|(n, _)| vyp_api::normalize_package_name(n) == name)
            {
                continue;
            }
            if let Some(source) = LocalSource::from_direct_reference(url, project_dir) {
                direct.push((name, source));
            }
        }
        self.local_sources.extend(direct);
    }

    /// Set the global `exclude-newer` cutoff (config, `--exclude-newer` or
    /// `VYP_EXCLUDE_NEWER`).
    pub fn set_exclude_newer(&mut self, value: &str) -> miette::Result<()> {
        self.exclude_newer.default = Some(parse_cutoff(value)?);
        Ok(())
    }

    pub fn from_file(path: &Path) -> miette::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| miette::miette!("Failed to read {}: {}", path.display(), e))?;
        let mut config = Self::from_str(&content)?;
        let project_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        config.resolve_local_source_paths(&project_dir);
        config.project_dir = Some(project_dir);
        Ok(config)
    }

    pub fn from_str(content: &str) -> miette::Result<Self> {
        let raw: RawPyProject = toml::from_str(content)
            .map_err(|e| miette::miette!("Failed to parse pyproject.toml: {}", e))?;

        let mut config = VypConfig::default();

        if let Some(project) = raw.project {
            config.dependencies = project.dependencies;
            config.optional_dependencies = project.optional_dependencies;
            config.requires_python = project.requires_python;
        }

        for (group_name, entries) in raw.dependency_groups {
            let mut deps = Vec::new();
            for entry in entries {
                match entry {
                    toml::Value::String(s) => deps.push(s),
                    toml::Value::Table(t) => {
                        if let Some(toml::Value::String(included)) = t.get("include-group") {
                            deps.push(format!("{{include-group: {}}}", included));
                        }
                    }
                    _ => {}
                }
            }
            config.dependency_groups.insert(group_name, deps);
        }

        let Some(tool) = raw.tool else {
            return Ok(config);
        };
        let Some(vyp) = tool.vyp else {
            return Ok(config);
        };

        // Resolution strategy
        if let Some(strategy) = &vyp.resolution_strategy {
            config.resolution_strategy = match strategy.as_str() {
                "lowest" => ResolutionStrategy::Lowest,
                "lowest-direct" => ResolutionStrategy::LowestDirect,
                _ => ResolutionStrategy::Highest,
            };
        }

        // Pre-release policy
        if let Some(pre) = &vyp.pre_releases {
            config.pre_releases = match pre.as_str() {
                "allow" => PreReleasePolicy::Allow,
                "if-necessary" => PreReleasePolicy::IfNecessary,
                _ => PreReleasePolicy::Disallow,
            };
        }

        // Index configuration
        if let Some(url) = vyp.index_url {
            config.index_url = url;
        }

        for raw_idx in vyp.extra_index {
            config.extra_indexes.push(IndexConfig {
                name: raw_idx.name,
                url: raw_idx.url,
                explicit: raw_idx.explicit,
            });
        }

        for (pkg, entries) in vyp.sources {
            let mut index_entries = Vec::new();
            for e in entries {
                match (e.index, e.path, e.git) {
                    (Some(index), None, None) => index_entries.push(SourceEntry { index, marker: e.marker }),
                    (None, Some(path), None) => config.local_sources.push((
                        pkg.clone(),
                        crate::build::sources::LocalSource::Path { path: PathBuf::from(path), editable: e.editable },
                    )),
                    (None, None, Some(url)) => config.local_sources.push((
                        pkg.clone(),
                        crate::build::sources::LocalSource::Git(crate::build::git::GitRef {
                            url,
                            rev: e.rev.or(e.tag).or(e.branch),
                            subdirectory: e.subdirectory,
                        }),
                    )),
                    _ => {
                        return Err(miette::miette!(
                            "[tool.vyp.sources] entry for {} must set exactly one of index, path or git",
                            pkg
                        ))
                    }
                }
            }
            if !index_entries.is_empty() {
                config.sources.insert(pkg, index_entries);
            }
        }

        // Torch backend
        config.torch_backend = vyp.torch_backend;

        // Universal resolution
        config.environments = vyp.environments;
        if let Some(ref s) = vyp.fork_strategy {
            config.fork_strategy = match s.as_str() {
                "fewest" => ForkStrategy::Fewest,
                _ => ForkStrategy::RequiresPython,
            };
        }

        // Overrides
        for raw_override in vyp.overrides {
            let mut dep_override =
                DependencyOverride::new(raw_override.package, raw_override.constraint)
                    .with_transitive(raw_override.transitive);
            if let Some(reason) = raw_override.reason {
                dep_override = dep_override.with_reason(reason);
            }
            config.overrides.push(dep_override);
        }

        config.python = vyp.python;
        config.startup_files.mode = match vyp.startup_files.as_deref() {
            None | Some("warn") => crate::cache::startup::StartupMode::Warn,
            Some("deny") => crate::cache::startup::StartupMode::Deny,
            Some("allow") => crate::cache::startup::StartupMode::Allow,
            Some(other) => {
                return Err(miette::miette!(
                    "Invalid startup-files value '{}': expected \"warn\", \"deny\" or \"allow\"",
                    other
                ))
            }
        };
        if let Some(s) = vyp.exclude_newer.as_deref() {
            config.set_exclude_newer(s)?;
        }
        for (pkg, value) in vyp.exclude_newer_package {
            let cutoff = match value {
                toml::Value::Boolean(false) => None,
                toml::Value::String(s) => Some(parse_cutoff(&s)?),
                other => {
                    return Err(miette::miette!(
                        "exclude-newer-package.{} must be false or a date/duration, not {}",
                        pkg,
                        other
                    ))
                }
            };
            config
                .exclude_newer
                .packages
                .insert(vyp_api::normalize_package_name(&pkg), cutoff);
        }
        config.pre_release_packages = vyp.pre_release_packages;
        config.sdists = match vyp.sdists.as_deref() {
            None | Some("fallback") => vyp_index::SdistPolicy::Fallback,
            Some("never") => vyp_index::SdistPolicy::Never,
            Some("allow") => vyp_index::SdistPolicy::Allow,
            Some(other) => {
                return Err(miette::miette!(
                    "Invalid sdists value '{}': expected \"never\", \"fallback\" or \"allow\"",
                    other
                ))
            }
        };
        config.startup_files.allowed = vyp
            .allow_startup_files
            .iter()
            .map(|n| vyp_api::normalize_package_name(n))
            .collect();

        // Errata feeds and inherited overrides
        config.errata = vyp.errata;
        config.inherit_overrides = match vyp.inherit_overrides {
            None | Some(RawInheritOverrides::Enabled(true)) => InheritOverrides::All,
            Some(RawInheritOverrides::Enabled(false)) => InheritOverrides::None,
            Some(RawInheritOverrides::Only(names)) => InheritOverrides::Only(
                names
                    .iter()
                    .map(|n| vyp_api::normalize_package_name(n))
                    .collect(),
            ),
        };

        // Substitutions
        for raw_sub in vyp.substitutions {
            let mut sub = SubstitutionSet::new(raw_sub.provides, raw_sub.packages);
            if let Some(pref) = raw_sub.prefer {
                sub = sub.with_preference(pref);
            }
            config.substitutions.push(sub);
        }

        // Plugins
        if let Some(raw_plugins) = vyp.plugins {
            config.plugin_search_paths = raw_plugins.search_paths;
            config.plugin_loads = raw_plugins
                .load
                .into_iter()
                .map(|p| PluginLoadConfig {
                    name: p.name,
                    path: p.path,
                    config: p.config,
                })
                .collect();
        }

        Ok(config)
    }

    /// Resolve the effective index URL for a package, considering sources
    /// routing and torch-backend auto-injection.
    #[allow(dead_code)]
    pub fn effective_index_for(&self, package: &str) -> Option<String> {
        let normalized = package.to_lowercase().replace('-', "_");

        // Check explicit sources first
        if let Some(entries) = self
            .sources
            .get(package)
            .or_else(|| self.sources.get(&normalized))
        {
            // For now, use the first entry (marker evaluation is a future refinement)
            if let Some(entry) = entries.first() {
                if let Some(idx) = self.extra_indexes.iter().find(|i| i.name == entry.index) {
                    return Some(idx.url.clone());
                }
            }
        }

        None
    }

    /// Package names of the project's direct dependencies (PEP 621
    /// dependencies, optional-dependency extras, and PEP 735 groups).
    ///
    /// Used to seed the index router's default-reachable set so that a named
    /// index does not capture a package that is also a direct or transitive
    /// dependency of an unscoped requirement.
    fn direct_dependency_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut push = |req_str: &String| {
            if let Ok(req) = req_str.parse::<Requirement>() {
                names.push(req.package.name().to_string());
            }
        };
        for dep in &self.dependencies {
            push(dep);
        }
        for extras in self.optional_dependencies.values() {
            for dep in extras {
                push(dep);
            }
        }
        for group in self.dependency_groups.values() {
            for dep in group {
                push(dep);
            }
        }
        names
    }

    /// Create metadata providers based on config.
    pub fn create_providers(
        &self,
        torch_backend_override: Option<&str>,
        marker_env: Option<&MarkerEnvironment>,
    ) -> miette::Result<Providers> {
        self.create_providers_with_client(torch_backend_override, marker_env, None)
    }

    /// Create providers with an optional shared HTTP client. When provided, all
    /// providers will share the same `reqwest::Client`, allowing TLS session
    /// reuse between resolution and installation.
    ///
    /// Returns the providers along with the [`IndexRouter`] seeded for them; the
    /// caller must hand the router to the [`ResolverBuilder`] so that named
    /// indexes are scoped to the transitive closure of their declared roots.
    pub fn create_providers_with_client(
        &self,
        torch_backend_override: Option<&str>,
        marker_env: Option<&MarkerEnvironment>,
        shared_client: Option<reqwest::Client>,
    ) -> miette::Result<Providers> {
        use crate::accelerator;

        let mut providers: Vec<Box<dyn vyp_api::MetadataProvider>> = Vec::new();
        let router = IndexRouter::new();

        // Resolve the active torch backend (if any) up front so its packages
        // can be excluded from the default-reachable seed.
        let backend_str = torch_backend_override
            .map(String::from)
            .or_else(|| self.torch_backend.clone());
        let torch_index: Option<(&'static str, Vec<String>)> =
            backend_str.as_ref().and_then(|val| {
                let resolved = if val == "auto" {
                    accelerator::detect_backend()
                } else {
                    accelerator::AcceleratorBackend::from_str(val)
                };
                accelerator::backend_index_url(&resolved).map(|url| {
                    let pkgs = accelerator::TORCH_PACKAGES
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    (url, pkgs)
                })
            });

        // Seed each named/explicit index with its declared root packages and
        // collect those roots so they are excluded from the default set.
        let mut routed: HashSet<String> = HashSet::new();
        for idx in &self.extra_indexes {
            if !idx.explicit {
                continue;
            }
            let roots: Vec<String> = self
                .sources
                .iter()
                .filter(|(_, entries)| entries.iter().any(|e| e.index == idx.name))
                .map(|(pkg, _)| pkg.clone())
                .collect();
            if roots.is_empty() {
                continue;
            }
            router.seed_index(&idx.name, &roots);
            for r in &roots {
                routed.insert(vyp_api::normalize_package_name(r));
            }
        }
        if let Some((_, ref torch_pkgs)) = torch_index {
            router.seed_index("pytorch", torch_pkgs);
            for r in torch_pkgs {
                routed.insert(vyp_api::normalize_package_name(r));
            }
        }

        // Seed the default-reachable set with direct dependencies that are not
        // routed to a named index. "Default index wins" on any later overlap.
        let default_seed: Vec<String> = self
            .direct_dependency_names()
            .into_iter()
            .filter(|n| !routed.contains(&vyp_api::normalize_package_name(n)))
            .collect();
        router.seed_default(&default_seed);

        let exclude_newer = Arc::new(self.exclude_newer.clone());
        let configure = |p: vyp_index::PyPIMetadataProvider| {
            let p = p.with_resolution_options(self.sdists, Arc::clone(&exclude_newer));
            match &self.sdist_builder {
                Some(hook) => p.with_sdist_builder(hook.0.clone()),
                None => p,
            }
        };

        // Primary index (unscoped — serves everything not claimed by a scope).
        providers.push(Box::new(configure(
            vyp_index::PyPIMetadataProvider::with_name_env_client(
                "pypi",
                &self.index_url,
                None,
                marker_env.cloned(),
                shared_client.clone(),
            ),
        )));

        // Extra indexes
        for idx in &self.extra_indexes {
            let scope = if idx.explicit {
                // Explicit indexes are scoped to their declared roots' closure.
                // Skip if nothing routes to this index.
                if !self
                    .sources
                    .values()
                    .any(|entries| entries.iter().any(|e| e.index == idx.name))
                {
                    continue;
                }
                Some(router.scope_for(&idx.name))
            } else {
                None
            };
            providers.push(Box::new(configure(
                vyp_index::PyPIMetadataProvider::with_name_env_client(
                    &idx.name,
                    &idx.url,
                    scope,
                    marker_env.cloned(),
                    shared_client.clone(),
                ),
            )));
        }

        // Torch backend: inject a scoped provider for the detected/configured backend.
        if let Some((url, _)) = torch_index {
            providers.push(Box::new(configure(
                vyp_index::PyPIMetadataProvider::with_name_env_client(
                    "pytorch",
                    url,
                    Some(router.scope_for("pytorch")),
                    marker_env.cloned(),
                    shared_client.clone(),
                ),
            )));
        }

        Ok((providers, router))
    }

    /// Convert this config's resolution strategy to the core enum.
    pub fn core_resolution_strategy(&self) -> vyp_core::ResolutionStrategy {
        match self.resolution_strategy {
            ResolutionStrategy::Highest => vyp_core::ResolutionStrategy::Highest,
            ResolutionStrategy::Lowest => vyp_core::ResolutionStrategy::Lowest,
            ResolutionStrategy::LowestDirect => vyp_core::ResolutionStrategy::LowestDirect,
        }
    }

    /// Convert this config's pre-release policy to the core enum.
    pub fn core_pre_release_policy(&self) -> vyp_core::PreReleasePolicy {
        match self.pre_releases {
            PreReleasePolicy::Allow => vyp_core::PreReleasePolicy::Allow,
            PreReleasePolicy::Disallow => vyp_core::PreReleasePolicy::Disallow,
            PreReleasePolicy::IfNecessary => vyp_core::PreReleasePolicy::IfNecessary,
        }
    }

    /// Load configured plugins into the given plugin loader.
    ///
    /// # Security
    ///
    /// Plugin loading executes arbitrary native code from shared libraries
    /// specified in `pyproject.toml`. Only load plugins from trusted sources.
    /// A compromised `pyproject.toml` could specify malicious shared libraries.
    /// Paths are validated to ensure they exist and are regular files or
    /// directories before loading.
    pub fn load_plugins(&self, loader: &mut vyp_core::plugin::loader::PluginLoader) {
        for search_path in &self.plugin_search_paths {
            let path = std::path::Path::new(search_path);
            if !path.is_dir() {
                tracing::warn!(
                    "Plugin search path '{}' is not a directory, skipping",
                    search_path
                );
                continue;
            }
            let errors = unsafe { loader.load_from_directory(path) };
            for e in errors {
                tracing::warn!("Plugin load error in {}: {}", search_path, e);
            }
        }

        for plugin_cfg in &self.plugin_loads {
            if let Some(ref path_str) = plugin_cfg.path {
                let path = std::path::Path::new(path_str);
                if !path.is_file() {
                    tracing::warn!(
                        "Plugin path '{}' for '{}' is not a file, skipping",
                        path_str,
                        plugin_cfg.name
                    );
                    continue;
                }
                if let Err(e) = unsafe { loader.load_plugin(path) } {
                    tracing::warn!("Failed to load plugin '{}': {}", plugin_cfg.name, e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(tool_vyp: &str) -> VypConfig {
        VypConfig::from_str(&format!("[tool.vyp]\n{}", tool_vyp)).unwrap()
    }

    #[test]
    fn inherit_overrides_defaults_to_all() {
        assert_eq!(parse("").inherit_overrides, InheritOverrides::All);
        assert_eq!(
            parse("inherit-overrides = false").inherit_overrides,
            InheritOverrides::None
        );
    }

    #[test]
    fn inherit_overrides_allowlist_is_normalized() {
        let config = parse("inherit-overrides = [\"Lib-A\"]");
        assert!(config.inherit_overrides.allows("lib_a"));
        assert!(!config.inherit_overrides.allows("lib_b"));
    }

    #[test]
    fn errata_feeds_are_parsed() {
        let config = parse("errata = [\"https://example.com/e.toml\", \"local.toml\"]");
        assert_eq!(
            config.errata,
            vec!["https://example.com/e.toml", "local.toml"]
        );
    }
}
