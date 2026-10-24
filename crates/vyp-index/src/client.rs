use std::collections::HashMap;

use vyp_api::traits::metadata_provider::{MetadataProvider, PackageMetadata, PackageVersions};
use vyp_api::{ConflictSet, Requirement, VypPackage, VypVersion};

/// An offline metadata provider for testing and mock scenarios.
/// All packages and their metadata must be registered before resolution.
#[derive(Debug, Clone)]
pub struct OfflineMetadataProvider {
    /// (package_name) -> sorted list of versions
    versions: HashMap<String, Vec<VypVersion>>,
    /// (package_name, version) -> metadata
    metadata: HashMap<(String, VypVersion), PackageMetadata>,
}

impl OfflineMetadataProvider {
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
            metadata: HashMap::new(),
        }
    }

    /// Register a package version with its dependencies.
    pub fn add_package(&mut self, name: &str, version: VypVersion, dependencies: Vec<Requirement>) {
        self.add_package_with_conflicts(name, version, dependencies, ConflictSet::new());
    }

    /// Alias for `add_package`; kept for backward compatibility with tests.
    pub fn add_package_raw(
        &mut self,
        name: &str,
        version: VypVersion,
        dependencies: Vec<Requirement>,
    ) {
        self.add_package(name, version, dependencies);
    }

    /// Register a package version with dependencies and conflict declarations.
    pub fn add_package_with_conflicts(
        &mut self,
        name: &str,
        version: VypVersion,
        dependencies: Vec<Requirement>,
        conflicts: ConflictSet,
    ) {
        let normalized = vyp_api::types::package::normalize_package_name(name);

        self.versions
            .entry(normalized.clone())
            .or_default()
            .push(version.clone());

        let metadata = PackageMetadata {
            package: VypPackage::named(name),
            version: version.clone(),
            dependencies,
            conflict_declarations: conflicts,
            source: "offline".to_string(),
        };

        self.metadata.insert((normalized, version), metadata);
    }
}

impl Default for OfflineMetadataProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MetadataProvider for OfflineMetadataProvider {
    fn name(&self) -> &str {
        "offline"
    }

    fn priority(&self) -> i32 {
        0
    }

    fn can_provide(&self, package: &VypPackage) -> bool {
        let name = vyp_api::types::package::normalize_package_name(package.name());
        self.versions.contains_key(&name)
    }

    fn available_versions(
        &self,
        package: &VypPackage,
    ) -> Result<Option<PackageVersions>, Box<dyn std::error::Error + Send + Sync>> {
        let name = vyp_api::types::package::normalize_package_name(package.name());
        match self.versions.get(&name) {
            Some(versions) => Ok(Some(PackageVersions {
                package: package.clone(),
                versions: versions.clone(),
            })),
            None => Ok(None),
        }
    }

    fn get_metadata(
        &self,
        package: &VypPackage,
        version: &VypVersion,
    ) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error + Send + Sync>> {
        let name = vyp_api::types::package::normalize_package_name(package.name());
        Ok(self.metadata.get(&(name, version.clone())).cloned())
    }
}
