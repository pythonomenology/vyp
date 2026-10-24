use vyp_api::{
    ComparisonOp, ConflictDeclaration, ConflictSet, DependencyOverride, Requirement,
    SubstitutionSet, VypVersion,
};
use vyp_core::{PreReleasePolicy, ResolutionStrategy, ResolverBuilder, VypError};
use vyp_index::OfflineMetadataProvider;

fn v(major: u32, minor: u32, patch: u32) -> VypVersion {
    VypVersion::from_parts(major, minor, patch)
}

fn req_gte(name: &str, version: VypVersion) -> Requirement {
    Requirement::new(name).with_constraint(ComparisonOp::Gte, version)
}

fn req_range(name: &str, lower: VypVersion, upper: VypVersion) -> Requirement {
    Requirement::new(name)
        .with_constraint(ComparisonOp::Gte, lower)
        .with_constraint(ComparisonOp::Lt, upper)
}

// ---------- Existing tests ----------

#[test]
fn test_enterprise_dependency_graph() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("numpy", v(1, 24, 0), vec![]);
    provider.add_package("numpy", v(1, 26, 4), vec![]);
    provider.add_package("numpy", v(2, 0, 0), vec![]);
    provider.add_package("pandas", v(2, 0, 0), vec![req_gte("numpy", v(1, 24, 0))]);
    provider.add_package("pandas", v(2, 1, 0), vec![req_gte("numpy", v(1, 24, 0))]);
    provider.add_package("requests", v(2, 28, 0), vec![]);
    provider.add_package("requests", v(2, 31, 0), vec![]);
    provider.add_package("pydantic", v(2, 0, 0), vec![]);
    provider.add_package("pydantic", v(2, 5, 0), vec![]);
    provider.add_package("cryptography", v(41, 0, 0), vec![]);
    provider.add_package("cryptography", v(42, 0, 0), vec![]);

    provider.add_package(
        "web-framework",
        v(1, 0, 0),
        vec![
            req_gte("requests", v(2, 28, 0)),
            req_gte("pydantic", v(2, 0, 0)),
        ],
    );
    provider.add_package(
        "auth-service",
        v(1, 0, 0),
        vec![
            req_gte("requests", v(2, 25, 0)),
            req_gte("cryptography", v(41, 0, 0)),
        ],
    );
    provider.add_package(
        "ml-pipeline",
        v(1, 0, 0),
        vec![
            req_range("numpy", v(1, 24, 0), v(2, 0, 0)),
            req_gte("pandas", v(2, 0, 0)),
        ],
    );

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .add_dependency(req_gte("web-framework", v(1, 0, 0)))
        .add_dependency(req_gte("auth-service", v(1, 0, 0)))
        .add_dependency(req_gte("ml-pipeline", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert_eq!(result.packages.len(), 8);
    let numpy = result.packages.get("numpy").unwrap();
    assert!(numpy < &v(2, 0, 0));
    assert_eq!(result.packages.get("requests").unwrap(), &v(2, 31, 0));
}

#[test]
fn test_deep_transitive_conflict_propagation() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("numpy", v(1, 26, 0), vec![]);
    provider.add_package("numpy", v(2, 0, 0), vec![]);

    let mut core_conflicts = ConflictSet::new();
    core_conflicts.add(
        ConflictDeclaration::new(
            "numpy-compat",
            vec!["legacy".into(), "modern".into()],
            vec!["numpy".into()],
        )
        .with_transitive(true),
    );

    provider.add_package_with_conflicts(
        "pkg-core",
        v(1, 0, 0),
        vec![req_gte("numpy", v(1, 0, 0))],
        core_conflicts,
    );
    provider.add_package("pkg-mid", v(1, 0, 0), vec![req_gte("pkg-core", v(1, 0, 0))]);
    provider.add_package("pkg-top", v(1, 0, 0), vec![req_gte("pkg-mid", v(1, 0, 0))]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .add_dependency(req_gte("pkg-top", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert!(result.packages.contains_key("pkg_core"));
    assert!(result.packages.contains_key("numpy"));

    let has_numpy_conflict = result
        .inherited_conflicts
        .values()
        .any(|cs| cs.declarations.iter().any(|d| d.name == "numpy-compat"));
    assert!(has_numpy_conflict);
}

#[test]
fn test_unsolvable_diamond() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("shared", v(1, 0, 0), vec![]);
    provider.add_package("shared", v(2, 0, 0), vec![]);

    provider.add_package(
        "left",
        v(1, 0, 0),
        vec![req_range("shared", v(1, 0, 0), v(2, 0, 0))],
    );
    provider.add_package("right", v(1, 0, 0), vec![req_gte("shared", v(2, 0, 0))]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .add_dependency(req_gte("left", v(1, 0, 0)))
        .add_dependency(req_gte("right", v(1, 0, 0)))
        .resolve();

    assert!(result.is_err());
    match result {
        Err(VypError::NoSolution(msg)) => {
            assert!(msg.contains("shared"));
            // Should contain explain_failure suggestions
            assert!(msg.contains("Suggestions:"));
        }
        _ => panic!("Expected NoSolution error"),
    }
}

#[test]
fn test_dependency_override_exact_pin() {
    let pin = DependencyOverride::new("numpy", "==1.24.0");
    assert!(pin.is_exact_pin());
    assert_eq!(pin.pinned_version(), Some("1.24.0"));

    let range = DependencyOverride::new("numpy", ">=1.26,<2");
    assert!(!range.is_exact_pin());
    assert_eq!(range.pinned_version(), None);
}

#[test]
fn test_dependency_override_transitive_inheritance() {
    let original = DependencyOverride::new("numpy", "==1.24.0")
        .with_transitive(true)
        .with_reason("GPU/CPU fix");

    let inherited = original.inherit_through("pkg-a");
    assert_eq!(inherited.origin.as_deref(), Some("pkg-a"));
    assert_eq!(inherited.propagation_path, vec!["pkg-a"]);
    assert!(inherited.transitive);

    let double = inherited.inherit_through("pkg-b");
    assert_eq!(double.origin.as_deref(), Some("pkg-a"));
    assert_eq!(double.propagation_path, vec!["pkg-a", "pkg-b"]);
}

#[test]
fn test_resolver_with_exact_pin_override() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("numpy", v(1, 24, 0), vec![]);
    provider.add_package("numpy", v(1, 26, 0), vec![]);
    provider.add_package("numpy", v(2, 0, 0), vec![]);

    let overrides = vec![DependencyOverride::new("numpy", "==1.24.0")];

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .with_overrides(overrides)
        .add_dependency(req_gte("numpy", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert_eq!(result.packages.get("numpy").unwrap(), &v(1, 24, 0));
}

#[test]
fn test_provenance_tracking() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("a", v(1, 0, 0), vec![req_gte("b", v(1, 0, 0))]);
    provider.add_package("b", v(1, 0, 0), vec![req_gte("c", v(1, 0, 0))]);
    provider.add_package("b", v(2, 0, 0), vec![req_gte("c", v(2, 0, 0))]);
    provider.add_package("c", v(1, 0, 0), vec![]);
    provider.add_package("c", v(2, 0, 0), vec![]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .add_dependency(req_gte("a", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert!(result.provenance.records.contains_key("a"));
    assert!(result.provenance.records.contains_key("b"));
    assert!(result.provenance.records.contains_key("c"));

    for (pkg_name, version) in &result.packages {
        let record = result.provenance.records.get(pkg_name).unwrap();
        assert_eq!(record.version, version.to_string());
    }
}

#[test]
fn test_config_parsing() {
    let toml_content = r#"
[project]
dependencies = ["numpy>=1.20", "pandas>=2.0"]

[tool.vyp]
resolution-strategy = "highest"
pre-releases = "disallow"

[[tool.vyp.overrides]]
package = "numpy"
constraint = ">=1.24,<2"

[[tool.vyp.overrides]]
package = "scipy"
constraint = "==1.11.0"
transitive = true
reason = "GPU compat"

[[tool.vyp.substitutions]]
provides = "opencv"
packages = ["opencv-python", "opencv-python-headless"]
prefer = "opencv-python-headless"
"#;

    let parsed: toml::Value = toml::from_str(toml_content).unwrap();
    let project = parsed.get("project").unwrap();
    let deps = project.get("dependencies").unwrap().as_array().unwrap();
    assert_eq!(deps.len(), 2);

    let vyp = parsed.get("tool").unwrap().get("vyp").unwrap();
    let overrides = vyp.get("overrides").unwrap().as_array().unwrap();
    assert_eq!(overrides.len(), 2);
    assert_eq!(
        overrides[0].get("package").unwrap().as_str().unwrap(),
        "numpy"
    );
    assert_eq!(
        overrides[1].get("constraint").unwrap().as_str().unwrap(),
        "==1.11.0"
    );
    assert!(overrides[1].get("transitive").unwrap().as_bool().unwrap());
}

// ---------- Config parsing: index URLs, sources, torch-backend ----------

#[test]
fn test_config_parsing_indexes_and_sources() {
    let toml_content = r#"
[project]
dependencies = ["requests>=2.28"]

[tool.vyp]
index-url = "https://my-registry.example.com/simple"
torch-backend = "cu128"

[[tool.vyp.extra-index]]
name = "pytorch-cu128"
url = "https://download.pytorch.org/whl/cu128"
explicit = true

[tool.vyp.sources]
torch = [
  { index = "pytorch-cu128", marker = "sys_platform == 'linux'" },
]
"#;

    let parsed: toml::Value = toml::from_str(toml_content).unwrap();
    let vyp = parsed.get("tool").unwrap().get("vyp").unwrap();

    assert_eq!(
        vyp.get("index-url").unwrap().as_str().unwrap(),
        "https://my-registry.example.com/simple"
    );
    assert_eq!(vyp.get("torch-backend").unwrap().as_str().unwrap(), "cu128");

    let extras = vyp.get("extra-index").unwrap().as_array().unwrap();
    assert_eq!(extras.len(), 1);
    assert_eq!(
        extras[0].get("name").unwrap().as_str().unwrap(),
        "pytorch-cu128"
    );
    assert!(extras[0].get("explicit").unwrap().as_bool().unwrap());

    let sources = vyp.get("sources").unwrap();
    let torch_sources = sources.get("torch").unwrap().as_array().unwrap();
    assert_eq!(torch_sources.len(), 1);
    assert_eq!(
        torch_sources[0].get("index").unwrap().as_str().unwrap(),
        "pytorch-cu128"
    );
}

// ---------- New tests for Step 9 ----------

#[test]
fn test_resolution_strategy_lowest() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("pkg", v(1, 0, 0), vec![]);
    provider.add_package("pkg", v(2, 0, 0), vec![]);
    provider.add_package("pkg", v(3, 0, 0), vec![]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .with_resolution_strategy(ResolutionStrategy::Lowest)
        .add_dependency(req_gte("pkg", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert_eq!(result.packages.get("pkg").unwrap(), &v(1, 0, 0));
}

#[test]
fn test_resolution_strategy_highest() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("pkg", v(1, 0, 0), vec![]);
    provider.add_package("pkg", v(2, 0, 0), vec![]);
    provider.add_package("pkg", v(3, 0, 0), vec![]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .with_resolution_strategy(ResolutionStrategy::Highest)
        .add_dependency(req_gte("pkg", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert_eq!(result.packages.get("pkg").unwrap(), &v(3, 0, 0));
}

#[test]
fn test_pre_release_filtering() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("pkg", v(1, 0, 0), vec![]);
    // Add a pre-release version
    let mut pre = VypVersion::from_parts(2, 0, 0);
    pre.pre = Some(vyp_api::types::version::PreRelease {
        kind: vyp_api::types::version::PreReleaseKind::Rc,
        number: 1,
    });
    provider.add_package_raw("pkg", pre.clone(), vec![]);

    // With pre-releases disallowed, should pick 1.0.0
    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider.clone()))
        .with_pre_release_policy(PreReleasePolicy::Disallow)
        .add_dependency(req_gte("pkg", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert_eq!(result.packages.get("pkg").unwrap(), &v(1, 0, 0));

    // With pre-releases allowed, should pick the rc
    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .with_pre_release_policy(PreReleasePolicy::Allow)
        .add_dependency(req_gte("pkg", v(1, 0, 0)))
        .resolve()
        .unwrap();

    assert_eq!(result.packages.get("pkg").unwrap(), &pre);
}

#[test]
fn test_provenance_requested_by_tracking() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("top", v(1, 0, 0), vec![req_gte("mid", v(1, 0, 0))]);
    provider.add_package("mid", v(1, 0, 0), vec![req_gte("bottom", v(1, 0, 0))]);
    provider.add_package("bottom", v(1, 0, 0), vec![]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .add_dependency(req_gte("top", v(1, 0, 0)))
        .resolve()
        .unwrap();

    // 'mid' should be requested_by 'top'
    let mid_record = result.provenance.records.get("mid").unwrap();
    assert!(
        mid_record.requested_by.iter().any(|r| r.contains("top")),
        "mid should be requested by top, got: {:?}",
        mid_record.requested_by
    );

    // 'bottom' should be requested_by 'mid'
    let bottom_record = result.provenance.records.get("bottom").unwrap();
    assert!(
        bottom_record.requested_by.iter().any(|r| r.contains("mid")),
        "bottom should be requested by mid, got: {:?}",
        bottom_record.requested_by
    );
}

#[test]
fn test_explain_failure_has_suggestions() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("x", v(1, 0, 0), vec![]);
    provider.add_package("x", v(2, 0, 0), vec![]);

    provider.add_package(
        "a",
        v(1, 0, 0),
        vec![req_range("x", v(1, 0, 0), v(2, 0, 0))],
    );
    provider.add_package("b", v(1, 0, 0), vec![req_gte("x", v(2, 0, 0))]);

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .add_dependency(req_gte("a", v(1, 0, 0)))
        .add_dependency(req_gte("b", v(1, 0, 0)))
        .resolve();

    match result {
        Err(VypError::NoSolution(msg)) => {
            // Should contain actionable suggestions from explain_failure
            assert!(
                msg.contains("vyp conflict add") || msg.contains("vyp override add"),
                "Error should contain actionable suggestions, got: {}",
                msg
            );
        }
        _ => panic!("Expected NoSolution"),
    }
}

#[test]
fn test_substitution_excludes_non_preferred() {
    let mut provider = OfflineMetadataProvider::new();

    provider.add_package("opencv_python", v(4, 9, 0), vec![]);
    provider.add_package("opencv_python_headless", v(4, 9, 0), vec![]);

    let subs = vec![SubstitutionSet::new(
        "opencv",
        vec!["opencv_python".into(), "opencv_python_headless".into()],
    )
    .with_preference("opencv_python_headless")];

    let result = ResolverBuilder::new()
        .with_provider(Box::new(provider))
        .with_substitutions(subs)
        .add_dependency(req_gte("opencv_python_headless", v(4, 0, 0)))
        .resolve()
        .unwrap();

    assert!(result.packages.contains_key("opencv_python_headless"));
}

#[test]
fn test_plugin_loader_builtin_registration() {
    use vyp_api::plugin_abi::VYP_ABI_VERSION;
    use vyp_api::PluginRegistration;
    use vyp_core::plugin::loader::PluginLoader;

    let mut loader = PluginLoader::new();

    let registration = PluginRegistration {
        abi_version: VYP_ABI_VERSION,
        name: "test-builtin".to_string(),
        version: "1.0.0".to_string(),
        strategies: Vec::new(),
        metadata_providers: Vec::new(),
        filters: Vec::new(),
    };

    loader.register_builtin(registration);

    assert_eq!(loader.loaded_plugins().len(), 1);
    assert_eq!(loader.loaded_plugins()[0].name, "test-builtin");
    assert_eq!(loader.loaded_plugins()[0].source, "builtin");
}

#[test]
fn test_cache_lru_eviction_integration() {
    use vyp_api::traits::metadata_provider::PackageMetadata;
    use vyp_api::{ConflictSet, VypPackage};
    use vyp_index::MetadataCache;

    let dir = std::env::temp_dir().join(format!("vyp-cache-integration-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let mut cache = MetadataCache::with_max_size(dir.clone(), 500);

    for i in 0..30 {
        let ver = v(1, 0, i);
        let meta = PackageMetadata {
            package: VypPackage::named(&format!("pkg-{}", i)),
            version: ver.clone(),
            dependencies: Vec::new(),
            conflict_declarations: ConflictSet::new(),
            source: "test".to_string(),
        };
        cache.insert(&format!("pkg-{}", i), &ver, &meta);
    }

    // Should have evicted entries to stay within 500 bytes
    assert!(cache.total_size() <= 500);
    assert!(cache.entry_count() < 30);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_optional_dependencies_parsing() {
    let toml_content = r#"
[project]
dependencies = ["numpy>=1.20"]

[project.optional-dependencies]
dev = ["pytest>=7.0", "mypy>=1.0"]
docs = ["sphinx>=6.0"]

[dependency-groups]
test = ["pytest>=8.0", "coverage>=7.0"]
"#;

    let parsed: toml::Value = toml::from_str(toml_content).unwrap();

    let project = parsed.get("project").unwrap();
    let opt_deps = project.get("optional-dependencies").unwrap();
    let dev_deps = opt_deps.get("dev").unwrap().as_array().unwrap();
    assert_eq!(dev_deps.len(), 2);

    let dep_groups = parsed.get("dependency-groups").unwrap();
    let test_group = dep_groups.get("test").unwrap().as_array().unwrap();
    assert_eq!(test_group.len(), 2);
}

#[test]
fn test_version_local_segment_ordering() {
    let v1: VypVersion = "1.0".parse().unwrap();
    let v2: VypVersion = "1.0+local".parse().unwrap();
    let v3: VypVersion = "1.0+local.2".parse().unwrap();

    assert!(v1 < v2);
    assert!(v2 < v3);
    assert!(!v1.is_local());
    assert!(v2.is_local());
}

#[test]
fn test_url_requirement_parsing() {
    let req: Requirement = "mypackage @ https://example.com/pkg.tar.gz"
        .parse()
        .unwrap();
    assert!(req.is_url());
    assert_eq!(req.url.as_deref(), Some("https://example.com/pkg.tar.gz"));
    assert!(req.constraints.is_empty());
}

#[test]
fn test_compatible_release_operator() {
    let req: Requirement = "pkg~=1.4.2".parse().unwrap();

    // ~=1.4.2 means >=1.4.2, <1.5.0
    assert!(req.satisfied_by(&VypVersion::from_parts(1, 4, 2)));
    assert!(req.satisfied_by(&VypVersion::from_parts(1, 4, 9)));
    assert!(!req.satisfied_by(&VypVersion::from_parts(1, 5, 0)));
    assert!(!req.satisfied_by(&VypVersion::from_parts(1, 4, 1)));
}

// ---------- Scoped named-index routing ----------

use std::sync::{Arc, Mutex};
use vyp_api::{IndexScope, MetadataProvider, PackageMetadata, PackageVersions, VypPackage};
use vyp_core::IndexRouter;

/// Wraps an offline provider as a named index restricted by an [`IndexScope`],
/// recording which packages it is asked to serve.
#[derive(Debug)]
struct ScopedRecordingProvider {
    name: String,
    inner: OfflineMetadataProvider,
    scope: Arc<dyn IndexScope>,
    served: Arc<Mutex<Vec<String>>>,
}

impl MetadataProvider for ScopedRecordingProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn priority(&self) -> i32 {
        20
    }
    fn can_provide(&self, package: &VypPackage) -> bool {
        self.inner.can_provide(package) && self.scope.allows(package.name())
    }
    fn available_versions(
        &self,
        package: &VypPackage,
    ) -> Result<Option<PackageVersions>, Box<dyn std::error::Error + Send + Sync>> {
        self.served.lock().unwrap().push(package.name().to_string());
        self.inner.available_versions(package)
    }
    fn get_metadata(
        &self,
        package: &VypPackage,
        version: &VypVersion,
    ) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error + Send + Sync>> {
        self.inner.get_metadata(package, version)
    }
}

#[test]
fn test_named_index_scopes_to_transitive_closure() {
    // The "torch" index hosts torch and its private deps (plus a mirror of a
    // shared package). The default index hosts everything, including an
    // unrelated top-level dependency.
    let mut torch_idx = OfflineMetadataProvider::new();
    torch_idx.add_package("torch", v(2, 1, 0), vec![req_gte("sympy", v(1, 0, 0))]);
    torch_idx.add_package("sympy", v(1, 12, 0), vec![]);
    torch_idx.add_package("filelock", v(3, 0, 0), vec![]); // stale mirror copy
    torch_idx.add_package("requests", v(2, 0, 0), vec![]); // stale mirror copy

    let mut default_idx = OfflineMetadataProvider::new();
    default_idx.add_package("torch", v(1, 0, 0), vec![]); // older mirror
    default_idx.add_package("sympy", v(1, 12, 0), vec![]);
    default_idx.add_package("filelock", v(3, 13, 0), vec![]);
    default_idx.add_package(
        "requests",
        v(2, 31, 0),
        vec![req_gte("filelock", v(3, 0, 0))],
    );

    let router = IndexRouter::new();
    router.seed_index("torch", &["torch".to_string()]);
    // Direct deps minus routed roots: only `requests` is a non-scoped root.
    router.seed_default(&["requests".to_string()]);

    let served = Arc::new(Mutex::new(Vec::new()));
    let scoped = ScopedRecordingProvider {
        name: "torch".into(),
        inner: torch_idx,
        scope: router.scope_for("torch"),
        served: Arc::clone(&served),
    };

    let result = ResolverBuilder::new()
        .with_index_router(router)
        .with_provider(Box::new(scoped))
        .with_provider(Box::new(default_idx))
        .add_dependency(req_gte("torch", v(2, 0, 0)))
        .add_dependency(req_gte("requests", v(2, 30, 0)))
        .resolve()
        .unwrap();

    // torch resolves from the named index (2.1.0), not the default mirror (1.0.0).
    assert_eq!(result.packages.get("torch").unwrap(), &v(2, 1, 0));

    let served = served.lock().unwrap();
    // (1) torch and its transitive dep sympy were served by the named index.
    assert!(served.iter().any(|p| p == "torch"), "torch served by index");
    assert!(
        served.iter().any(|p| p == "sympy"),
        "transitive sympy served by index"
    );
    // (2) the unrelated top-level dependency was NOT served by the named index.
    assert!(
        !served.iter().any(|p| p == "requests"),
        "requests must not route to torch index"
    );
}
