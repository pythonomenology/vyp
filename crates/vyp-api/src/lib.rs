pub mod plugin_abi;
pub mod traits;
pub mod types;

pub use plugin_abi::{PluginRegistration, VYP_ABI_VERSION};
pub use traits::conflict_strategy::{
    ConflictContext, ConflictStrategy, ConflictSuggestion, ForkSpec, RangeRewrite, StrategyVerdict,
};
pub use traits::index_scope::IndexScope;
pub use traits::metadata_provider::{
    MetadataProvider, PackageMetadata, PackageVersions, WheelDist,
};
pub use traits::resolution_filter::{Candidate, ResolutionFilter};
pub use types::conflict::{ConflictDeclaration, ConflictSet};
pub use types::marker::{MarkerEnvironment, MarkerOp, MarkerTree, MarkerValue, MarkerVar};
pub use types::override_layer::DependencyOverride;
pub use types::package::{normalize_package_name, VypPackage};
pub use types::provenance::{ProvenanceRecord, ResolutionProvenance, SelectionReason};
pub use types::requirement::{ComparisonOp, Requirement, RequirementParseError, VersionConstraint};
pub use types::substitution::SubstitutionSet;
pub use types::variant::{VariantDescriptor, VariantMetadata, VariantPriorities, VariantProperty};
pub use types::version::VypVersion;
