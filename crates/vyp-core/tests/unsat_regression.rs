//! Regression tests for transitively-unsatisfiable root requirements.
//!
//! A backtrack to decision level 0 used to remove the root decision (made at
//! level 1), after which the solver forgot the project's own requirements and
//! returned an empty `Ok({})` instead of reporting that no solution exists.

use vyp_api::{ComparisonOp, Requirement, VypVersion};
use vyp_core::{ResolverBuilder, VypError};
use vyp_index::OfflineMetadataProvider;

fn v(n: u32) -> VypVersion {
    VypVersion::from_parts(n, 0, 0)
}
fn lt(name: &str, n: u32) -> Requirement {
    Requirement::new(name).with_constraint(ComparisonOp::Lt, v(n))
}
fn gte(name: &str, n: u32) -> Requirement {
    Requirement::new(name).with_constraint(ComparisonOp::Gte, v(n))
}

fn assert_no_solution(provider: OfflineMetadataProvider, roots: Vec<Requirement>) {
    let mut b = ResolverBuilder::new().with_provider(Box::new(provider));
    for r in roots {
        b = b.add_dependency(r);
    }
    match b.resolve() {
        Err(VypError::NoSolution(_)) => {}
        Ok(res) => panic!("expected NoSolution, got Ok({:?})", res.packages),
        Err(e) => panic!("expected NoSolution, got error: {}", e),
    }
}

#[test]
fn direct_unsatisfiable_root() {
    let mut p = OfflineMetadataProvider::new();
    p.add_package("p0", v(1), vec![]);
    // root -> p0 < 1.0.0 (no version qualifies)
    assert_no_solution(p, vec![lt("p0", 1)]);
}

#[test]
fn transitively_unsatisfiable_root() {
    let mut p = OfflineMetadataProvider::new();
    p.add_package("p0", v(1), vec![]);
    p.add_package("p1", v(1), vec![lt("p0", 1)]); // p1's only version needs impossible p0
                                                  // root -> p1 ; resolving p1 forces an impossible p0, which must bubble up.
    assert_no_solution(p, vec![Requirement::new("p1")]);
}

#[test]
fn deep_transitive_unsat_two_roots() {
    let mut p = OfflineMetadataProvider::new();
    for n in 1..=3 {
        p.add_package("p0", v(n), vec![]);
        p.add_package("p2", v(n), vec![]);
    }
    p.add_package("p1", v(1), vec![lt("p0", 3), lt("p0", 1)]);
    p.add_package("p2", v(3), vec![Requirement::new("p1")]);
    p.add_package("p3", v(1), vec![lt("p0", 1), lt("p2", 1)]);
    p.add_package("p3", v(2), vec![Requirement::new("p2"), gte("p1", 1)]);
    assert_no_solution(p, vec![lt("p3", 3), gte("p2", 2)]);
}
