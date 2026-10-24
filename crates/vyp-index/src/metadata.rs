use vyp_api::{Requirement, VypPackage, VypVersion};

/// Parsed metadata from a distribution's METADATA file or pyproject.toml.
#[derive(Debug, Clone)]
pub struct DistributionMetadata {
    /// Canonical package name.
    pub name: String,
    /// Package version.
    pub version: VypVersion,
    /// `Requires-Dist` entries (PEP 508 dependencies).
    pub requires_dist: Vec<Requirement>,
    /// `Requires-Python` version specifier (e.g. `>=3.8`).
    pub requires_python: Option<String>,
    /// One-line package description from `Summary`.
    pub summary: Option<String>,
    /// SPDX license identifier from `License`.
    pub license: Option<String>,
}

impl DistributionMetadata {
    /// Convert the metadata name into a `VypPackage`.
    pub fn package(&self) -> VypPackage {
        VypPackage::named(&self.name)
    }
}
