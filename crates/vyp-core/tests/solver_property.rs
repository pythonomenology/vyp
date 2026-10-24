//! Randomized differential test for the two-watched-literals solver.
//!
//! Generates thousands of small dependency universes and checks that the
//! resolver's SAT/UNSAT verdict — and any solution it returns — agrees with an
//! independent brute-force search. This guards the watched-literal propagation
//! against the dangerous failure modes (a wrong solution, or a false
//! "no solution") that ordinary example-based tests can miss.

use vyp_api::{ComparisonOp, Requirement, VypVersion};
use vyp_core::resolver::{requirements_to_range, VS};
use vyp_core::{ResolverBuilder, VypError};
use vyp_index::OfflineMetadataProvider;

/// Deterministic LCG so any failure reproduces from its seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone)]
struct PkgVersion {
    version: VypVersion,
    deps: Vec<Requirement>,
}

struct Universe {
    /// packages[i] = the available versions of package "p{i}".
    packages: Vec<Vec<PkgVersion>>,
    roots: Vec<Requirement>,
}

fn name(i: usize) -> String {
    format!("p{}", i)
}

fn ver(n: u64) -> VypVersion {
    VypVersion::from_parts(n as u32, 0, 0)
}

/// Build a random universe of up to 5 packages, each with 1–3 versions and
/// 0–2 dependencies on other packages constrained by a `>=`/`<` bound.
fn gen_universe(rng: &mut Rng) -> Universe {
    let n_pkgs = 2 + rng.below(4) as usize; // 2..=5
    let mut packages = Vec::new();

    for _ in 0..n_pkgs {
        let n_vers = 1 + rng.below(3); // 1..=3
        let mut versions = Vec::new();
        for v in 0..n_vers {
            versions.push(PkgVersion {
                version: ver(v + 1), // 1.0.0 ..= 3.0.0
                deps: Vec::new(),
            });
        }
        packages.push(versions);
    }

    // Add random dependencies (only to packages of a lower index, to keep
    // graphs mostly acyclic but still exercise transitive chains).
    for i in 0..n_pkgs {
        for vi in 0..packages[i].len() {
            let n_deps = rng.below(3); // 0..=2
            for _ in 0..n_deps {
                if i == 0 {
                    break;
                }
                let target = rng.below(i as u64) as usize;
                let req = rand_req(rng, target);
                packages[i][vi].deps.push(req);
            }
        }
    }

    // Roots: require 1–2 of the packages.
    let n_roots = 1 + rng.below(2) as usize;
    let mut roots = Vec::new();
    for _ in 0..n_roots {
        let target = rng.below(n_pkgs as u64) as usize;
        roots.push(rand_req(rng, target));
    }

    Universe { packages, roots }
}

/// A random requirement on package `target`: unconstrained, `>= b`, or `< b`.
fn rand_req(rng: &mut Rng, target: usize) -> Requirement {
    let b = 1 + rng.below(3); // bound in 1..=3
    match rng.below(3) {
        0 => Requirement::new(&name(target)),
        1 => Requirement::new(&name(target)).with_constraint(ComparisonOp::Gte, ver(b)),
        _ => Requirement::new(&name(target)).with_constraint(ComparisonOp::Lt, ver(b)),
    }
}

fn range_of(req: &Requirement) -> VS {
    requirements_to_range(&req.constraints)
}

/// Brute-force whether a valid closed assignment exists: every package is
/// either absent or pinned to one of its versions, all roots are satisfied,
/// and every present package's dependencies are satisfied by present packages.
fn brute_force_sat(u: &Universe) -> bool {
    let n = u.packages.len();
    // state[i] in 0..=len(versions): 0 = absent, k = version index k-1.
    let mut state = vec![0usize; n];
    loop {
        if assignment_valid(u, &state) {
            return true;
        }
        // increment mixed-radix counter
        let mut i = 0;
        loop {
            if i == n {
                return false;
            }
            let radix = u.packages[i].len() + 1;
            state[i] += 1;
            if state[i] < radix {
                break;
            }
            state[i] = 0;
            i += 1;
        }
    }
}

fn assignment_valid(u: &Universe, state: &[usize]) -> bool {
    let present = |i: usize| -> Option<&PkgVersion> {
        match state[i] {
            0 => None,
            k => Some(&u.packages[i][k - 1]),
        }
    };

    // Roots satisfied.
    for req in &u.roots {
        let idx: usize = req.package.name()[1..].parse().unwrap();
        match present(idx) {
            Some(pv) if range_of(req).contains(&pv.version) => {}
            _ => return false,
        }
    }
    // Every present package's deps satisfied by present packages.
    for i in 0..u.packages.len() {
        if let Some(pv) = present(i) {
            for dep in &pv.deps {
                let didx: usize = dep.package.name()[1..].parse().unwrap();
                match present(didx) {
                    Some(dv) if range_of(dep).contains(&dv.version) => {}
                    _ => return false,
                }
            }
        }
    }
    true
}

/// Independently validate a solution the resolver returned.
fn validate_solution(u: &Universe, packages: &std::collections::HashMap<String, VypVersion>) {
    for req in &u.roots {
        let v = packages
            .get(req.package.name())
            .unwrap_or_else(|| panic!("root {} missing from solution", req.package.name()));
        assert!(
            range_of(req).contains(v),
            "root {} version {} out of range",
            req.package.name(),
            v
        );
    }
    for (pname, pver) in packages {
        let idx: usize = pname[1..].parse().unwrap();
        let pv = u.packages[idx]
            .iter()
            .find(|pv| &pv.version == pver)
            .unwrap_or_else(|| panic!("solution has unknown version {}=={}", pname, pver));
        for dep in &pv.deps {
            let v = packages
                .get(dep.package.name())
                .unwrap_or_else(|| panic!("dep {} of {} missing", dep.package.name(), pname));
            assert!(
                range_of(dep).contains(v),
                "dep {} version {} out of range",
                dep.package.name(),
                v
            );
        }
    }
}

#[test]
fn solver_matches_brute_force() {
    let mut checked_sat = 0;
    let mut checked_unsat = 0;

    for seed in 0..3000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1));
        let u = gen_universe(&mut rng);

        let mut provider = OfflineMetadataProvider::new();
        for (i, versions) in u.packages.iter().enumerate() {
            for pv in versions {
                provider.add_package(&name(i), pv.version.clone(), pv.deps.clone());
            }
        }

        let mut builder = ResolverBuilder::new().with_provider(Box::new(provider));
        for req in &u.roots {
            builder = builder.add_dependency(req.clone());
        }

        let result = builder.resolve();
        let expected_sat = brute_force_sat(&u);

        match result {
            Ok(res) => {
                assert!(
                    expected_sat,
                    "seed {}: resolver found a solution but brute force says UNSAT",
                    seed
                );
                validate_solution(&u, &res.packages);
                checked_sat += 1;
            }
            Err(VypError::NoSolution(_)) => {
                assert!(
                    !expected_sat,
                    "seed {}: resolver reported NO solution but one exists",
                    seed
                );
                checked_unsat += 1;
            }
            Err(other) => panic!("seed {}: unexpected resolver error: {}", seed, other),
        }
    }

    // Sanity: the corpus actually exercises both outcomes.
    assert!(checked_sat > 100, "too few SAT cases: {}", checked_sat);
    assert!(
        checked_unsat > 100,
        "too few UNSAT cases: {}",
        checked_unsat
    );
}
