use vyp_api::traits::conflict_strategy::ConflictStrategy;
use vyp_api::traits::metadata_provider::MetadataProvider;
use vyp_api::traits::resolution_filter::ResolutionFilter;

/// Registry of conflict strategies, sorted by priority (highest first).
///
/// The resolver consults strategies in priority order when a conflict
/// is detected. The first strategy that returns a non-`Abstain` verdict
/// wins.
#[derive(Default)]
pub struct StrategyRegistry {
    strategies: Vec<Box<dyn ConflictStrategy>>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a strategy. The registry re-sorts after each insertion
    /// so that higher-priority strategies are consulted first.
    pub fn register(&mut self, strategy: Box<dyn ConflictStrategy>) {
        self.strategies.push(strategy);
        self.strategies
            .sort_by_key(|b| std::cmp::Reverse(b.priority()));
    }

    /// Iterate over registered strategies in priority order.
    pub fn strategies(&self) -> &[Box<dyn ConflictStrategy>] {
        &self.strategies
    }
}

/// Registry of metadata providers, sorted by priority (highest first).
///
/// During resolution, providers are queried in priority order for each
/// package. The first provider that can serve a package wins.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Box<dyn MetadataProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider. The registry re-sorts after each insertion.
    pub fn register(&mut self, provider: Box<dyn MetadataProvider>) {
        self.providers.push(provider);
        self.providers
            .sort_by_key(|b| std::cmp::Reverse(b.priority()));
    }

    /// Iterate over registered providers in priority order.
    pub fn providers(&self) -> &[Box<dyn MetadataProvider>] {
        &self.providers
    }

    /// Hint to all providers that these packages will be needed soon.
    pub fn batch_prefetch(&self, packages: &[String]) {
        for provider in &self.providers {
            provider.prefetch(packages);
        }
    }

    /// Non-blocking: check if version data is already available for this package.
    pub fn try_available_versions(
        &self,
        package: &vyp_api::VypPackage,
    ) -> Option<vyp_api::traits::metadata_provider::PackageVersions> {
        for provider in &self.providers {
            if provider.can_provide(package) {
                if let Some(pvs) = provider.try_available_versions(package) {
                    return Some(pvs);
                }
            }
        }
        None
    }

    /// Return the best wheel URL for a resolved package+version.
    pub fn wheel_url(
        &self,
        package: &str,
        version: &vyp_api::VypVersion,
    ) -> Option<(String, String)> {
        for provider in &self.providers {
            if let Some(info) = provider.wheel_url(package, version) {
                return Some(info);
            }
        }
        None
    }

    /// Return the best wheel distribution (with integrity hashes) for a
    /// resolved package+version.
    pub fn sdist_dist(
        &self,
        package: &str,
        version: &vyp_api::VypVersion,
    ) -> Option<vyp_api::WheelDist> {
        self.providers
            .iter()
            .find_map(|p| p.sdist_dist(package, version))
    }

    pub fn wheel_dist(
        &self,
        package: &str,
        version: &vyp_api::VypVersion,
    ) -> Option<vyp_api::WheelDist> {
        for provider in &self.providers {
            if let Some(dist) = provider.wheel_dist(package, version) {
                return Some(dist);
            }
        }
        None
    }

    /// Collect profile data from all providers (merges counters).
    pub fn collect_profile_data(&self) -> std::collections::HashMap<String, usize> {
        let mut merged = std::collections::HashMap::new();
        for provider in &self.providers {
            for (k, v) in provider.profile_data() {
                *merged.entry(k).or_insert(0) += v;
            }
        }
        merged
    }
}

/// Registry of resolution filters, sorted by priority (highest first).
///
/// Filters run after version candidates are gathered but before the
/// solver picks a version. They can exclude candidates (e.g., pre-releases)
/// or annotate them with exclusion reasons.
#[derive(Default)]
pub struct FilterRegistry {
    filters: Vec<Box<dyn ResolutionFilter>>,
}

impl FilterRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a filter. The registry re-sorts after each insertion.
    pub fn register(&mut self, filter: Box<dyn ResolutionFilter>) {
        self.filters.push(filter);
        self.filters
            .sort_by_key(|b| std::cmp::Reverse(b.priority()));
    }

    /// Iterate over registered filters in priority order.
    pub fn filters(&self) -> &[Box<dyn ResolutionFilter>] {
        &self.filters
    }
}
