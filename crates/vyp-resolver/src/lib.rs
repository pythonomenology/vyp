//! Pure PubGrub dependency resolver algorithm.
//!
//! This crate contains the core solver and version-set conversion logic.
//! Orchestration (ResolverBuilder, plugin loader, strategies, metadata providers)
//! lives in `vyp-core`.

pub mod provider;
pub mod solver;

pub use provider::{requirements_to_range, VypVS};
pub use solver::{
    DecisionLevel, IncompatId, IncompatKind, Incompatibility, PackageId, PartialSolution,
    SatisfierInfo, SolverError, SolverState, Term, VsidsScoring, VS,
};
