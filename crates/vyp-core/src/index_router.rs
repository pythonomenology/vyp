//! Dynamic, transitive index routing.
//!
//! [`IndexRouter`] decides which named index serves each package during
//! resolution. It implements scoped routing for named/explicit indexes
//! (e.g. a PyTorch wheel index attached to the `torch` dependency): the index
//! serves its declared root packages *and their transitive closure*, but is
//! never consulted for unrelated top-level dependencies.
//!
//! # Overlap policy: the default index wins
//!
//! A package that is reachable from both a scoped root and an ordinary
//! dependency is served by the *default* index (PyPI), not the named index.
//! Only packages reachable **exclusively** through a named index's roots are
//! routed there. The declared root packages themselves are always authoritative
//! to their index regardless of overlap.
//!
//! # Best-effort discovery
//!
//! Membership is discovered lazily as the solver walks dependency edges (see
//! [`IndexRouter::propagate`]). The two membership sets only ever grow, which is
//! safe under PubGrub backtracking. In rare diamond-shaped graphs a shared
//! package may be observed before its non-scoped path is walked; to force a
//! routing decision in those cases, pin the package explicitly in
//! `[tool.vyp.sources]`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use vyp_api::{normalize_package_name, IndexScope};

#[derive(Default)]
struct RouterInner {
    /// Per-index transitive closure of its declared roots (grows during solve).
    scopes: HashMap<String, HashSet<String>>,
    /// Per-index declared root packages from `[tool.vyp.sources]` — authoritative.
    seeds: HashMap<String, HashSet<String>>,
    /// Packages reachable from non-scoped (default-index) dependencies.
    default_reachable: HashSet<String>,
}

/// Routes packages to named indexes by transitive reachability.
///
/// Shared (via [`Arc`]) between the providers that read routing decisions in
/// `can_provide` and the resolver loop that grows membership via
/// [`propagate`](IndexRouter::propagate).
#[derive(Default)]
pub struct IndexRouter {
    inner: Mutex<RouterInner>,
}

impl IndexRouter {
    /// Create an empty router that routes nothing to any named index.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Declare the root packages routed to `index` via `[tool.vyp.sources]`.
    ///
    /// Roots are authoritative: they always resolve from their named index and
    /// also seed the transitive closure that grows during resolution.
    pub fn seed_index(&self, index: &str, packages: &[String]) {
        let mut inner = self.inner.lock().expect("poisoned");
        for pkg in packages {
            let n = normalize_package_name(pkg);
            inner
                .seeds
                .entry(index.to_string())
                .or_default()
                .insert(n.clone());
            inner.scopes.entry(index.to_string()).or_default().insert(n);
        }
    }

    /// Mark packages as reachable from the default index (non-scoped roots).
    ///
    /// Typically seeded with the project's direct dependencies minus any
    /// packages routed to a named index.
    pub fn seed_default(&self, packages: &[String]) {
        let mut inner = self.inner.lock().expect("poisoned");
        for pkg in packages {
            inner.default_reachable.insert(normalize_package_name(pkg));
        }
    }

    /// Propagate membership from a resolved package to its dependencies.
    ///
    /// For every named index whose scope already contains `parent`, each
    /// dependency joins that scope. If `parent` is default-reachable, its
    /// dependencies become default-reachable too. Idempotent and cheap; called
    /// once per package expansion.
    pub fn propagate(&self, parent: &str, deps: &[String]) {
        if deps.is_empty() {
            return;
        }
        let parent = normalize_package_name(parent);
        let dep_names: Vec<String> = deps.iter().map(|d| normalize_package_name(d)).collect();

        let mut inner = self.inner.lock().expect("poisoned");

        let scoped_indexes: Vec<String> = inner
            .scopes
            .iter()
            .filter(|(_, set)| set.contains(&parent))
            .map(|(name, _)| name.clone())
            .collect();
        for idx in scoped_indexes {
            let set = inner.scopes.get_mut(&idx).expect("present");
            for d in &dep_names {
                set.insert(d.clone());
            }
        }

        if inner.default_reachable.contains(&parent) {
            for d in &dep_names {
                inner.default_reachable.insert(d.clone());
            }
        }
    }

    /// Whether `index` is allowed to serve `package` under the overlap policy.
    pub fn routes_to(&self, index: &str, package: &str) -> bool {
        let package = normalize_package_name(package);
        let inner = self.inner.lock().expect("poisoned");

        // Declared roots are always authoritative to their index.
        if inner
            .seeds
            .get(index)
            .is_some_and(|seeds| seeds.contains(&package))
        {
            return true;
        }

        // Transitive members route here only if not also default-reachable.
        match inner.scopes.get(index) {
            Some(scope) => scope.contains(&package) && !inner.default_reachable.contains(&package),
            None => false,
        }
    }

    /// Return an [`IndexScope`] view bound to `index` for a provider to hold.
    pub fn scope_for(self: &Arc<Self>, index: &str) -> Arc<dyn IndexScope> {
        Arc::new(IndexScopeView {
            router: Arc::clone(self),
            name: index.to_string(),
        })
    }
}

/// A per-index handle that delegates `allows` back to the shared router.
#[derive(Debug)]
struct IndexScopeView {
    router: Arc<IndexRouter>,
    name: String,
}

impl std::fmt::Debug for IndexRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexRouter").finish_non_exhaustive()
    }
}

impl IndexScope for IndexScopeView {
    fn allows(&self, package: &str) -> bool {
        self.router.routes_to(&self.name, package)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_root_is_authoritative() {
        let router = IndexRouter::new();
        router.seed_index("torch", &["torch".into()]);
        assert!(router.routes_to("torch", "torch"));
        assert!(!router.routes_to("torch", "numpy"));
    }

    #[test]
    fn transitive_deps_join_scope() {
        let router = IndexRouter::new();
        router.seed_index("torch", &["torch".into()]);
        router.propagate("torch", &["sympy".into(), "filelock".into()]);
        assert!(router.routes_to("torch", "sympy"));
        assert!(router.routes_to("torch", "filelock"));
        // Second hop.
        router.propagate("sympy", &["mpmath".into()]);
        assert!(router.routes_to("torch", "mpmath"));
    }

    #[test]
    fn default_wins_on_overlap() {
        let router = IndexRouter::new();
        router.seed_index("torch", &["torch".into()]);
        router.seed_default(&["requests".into()]);
        // torch pulls in filelock and sympy.
        router.propagate("torch", &["sympy".into(), "filelock".into()]);
        // requests also pulls in filelock.
        router.propagate("requests", &["filelock".into()]);
        // filelock is shared -> default (PyPI) wins.
        assert!(!router.routes_to("torch", "filelock"));
        // sympy is exclusive to torch -> stays.
        assert!(router.routes_to("torch", "sympy"));
    }

    #[test]
    fn unrelated_top_level_dep_not_routed() {
        let router = IndexRouter::new();
        router.seed_index("torch", &["torch".into()]);
        router.seed_default(&["requests".into()]);
        router.propagate("requests", &["urllib3".into(), "certifi".into()]);
        assert!(!router.routes_to("torch", "requests"));
        assert!(!router.routes_to("torch", "urllib3"));
        assert!(!router.routes_to("torch", "certifi"));
    }

    #[test]
    fn name_normalization() {
        let router = IndexRouter::new();
        router.seed_index("torch", &["Torch".into()]);
        assert!(router.routes_to("torch", "torch"));
        router.propagate("torch", &["NVIDIA-CUDA.Runtime".into()]);
        assert!(router.routes_to("torch", "nvidia_cuda_runtime"));
    }

    #[test]
    fn empty_router_routes_nothing() {
        let router = IndexRouter::new();
        assert!(!router.routes_to("anything", "torch"));
    }
}
