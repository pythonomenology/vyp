use crate::types::conflict::ConflictDeclaration;
use crate::types::package::VypPackage;
use crate::types::provenance::SelectionReason;
use crate::types::version::VypVersion;
use std::collections::HashMap;
use std::fmt::Debug;

/// Context provided to a ConflictStrategy when a conflict is detected.
#[derive(Debug, Clone)]
pub struct ConflictContext {
    /// The package that has conflicting requirements.
    pub contested_package: VypPackage,
    /// The conflicting requirements: (requesting_package, required_range_display).
    pub requirements: Vec<(String, String)>,
    /// Conflict declarations inherited from the dependency graph.
    pub inherited_conflicts: Vec<ConflictDeclaration>,
    /// The current set of resolved packages (snapshot).
    pub current_resolution: HashMap<String, VypVersion>,
}

/// The verdict returned by a ConflictStrategy.
#[derive(Debug, Clone)]
pub enum StrategyVerdict {
    /// This strategy does not handle this conflict; pass to the next strategy.
    Abstain,
    /// The conflict should be surfaced as an error (no resolution possible).
    Fail { message: String },
    /// Rewrite the version ranges to resolve the conflict.
    RewriteRanges { rewrites: Vec<RangeRewrite> },
    /// Fork the resolution: resolve the contested package differently
    /// depending on which conflict group is active.
    Fork {
        conflict_name: String,
        forks: Vec<ForkSpec>,
    },
}

/// Instruction to rewrite a version range for a package.
#[derive(Debug, Clone)]
pub struct RangeRewrite {
    pub package: String,
    pub new_lower: Option<VypVersion>,
    pub new_upper: Option<VypVersion>,
    pub upper_inclusive: bool,
}

/// A fork specification: which conflict side gets which version range.
#[derive(Debug, Clone)]
pub struct ForkSpec {
    pub side: String,
    pub lower: Option<VypVersion>,
    pub upper: Option<VypVersion>,
    pub upper_inclusive: bool,
}

/// A suggestion to show the user when resolution fails.
#[derive(Debug, Clone)]
pub struct ConflictSuggestion {
    pub source: String,
    pub message: String,
    pub command: Option<String>,
}

/// The primary plugin extension point for conflict resolution.
///
/// When the resolver encounters conflicting requirements for a package,
/// it consults registered strategies in priority order. A strategy can:
/// - Abstain (pass to next strategy)
/// - Fail with a message
/// - Rewrite version ranges
/// - Fork the resolution
///
/// Built-in strategies (transitive fork, override, fail) implement this
/// same trait and serve as reference implementations.
pub trait ConflictStrategy: Send + Sync + Debug {
    /// Human-readable name of this strategy.
    fn name(&self) -> &str;

    /// Priority order (higher = consulted first). Built-ins use 0-100.
    fn priority(&self) -> i32;

    /// Evaluate a conflict and return a verdict.
    fn evaluate(&self, context: &ConflictContext) -> StrategyVerdict;

    /// Provide suggestions when resolution fails entirely.
    fn suggest(&self, context: &ConflictContext) -> Vec<ConflictSuggestion> {
        let _ = context;
        Vec::new()
    }

    /// The provenance reason to record when this strategy rewrites or forks.
    /// Override this for built-in strategies to avoid brittle string matching.
    fn selection_reason(&self) -> SelectionReason {
        SelectionReason::ConflictResolution
    }
}
