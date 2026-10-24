use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use indexmap::IndexMap;
use version_ranges::Ranges;

use vyp_api::{VypPackage, VypVersion};

// ---------------------------------------------------------------------------
// PackageId / IncompatId — integer handles for the hot loop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PackageId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IncompatId(u32);

pub type VS = Ranges<VypVersion>;

// ---------------------------------------------------------------------------
// Term — Positive / Negative version set
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Term {
    Positive(VS),
    Negative(VS),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Satisfied,
    Contradicted,
    Inconclusive,
}

impl Term {
    pub fn any() -> Self {
        Term::Negative(Ranges::empty())
    }

    pub fn is_positive(&self) -> bool {
        matches!(self, Term::Positive(_))
    }

    pub fn negate(&self) -> Self {
        match self {
            Term::Positive(r) => Term::Negative(r.clone()),
            Term::Negative(r) => Term::Positive(r.clone()),
        }
    }

    pub fn intersection(&self, other: &Term) -> Term {
        match (self, other) {
            (Term::Positive(a), Term::Positive(b)) => Term::Positive(a.intersection(b)),
            (Term::Positive(p), Term::Negative(n)) | (Term::Negative(n), Term::Positive(p)) => {
                Term::Positive(p.intersection(&n.complement()))
            }
            (Term::Negative(a), Term::Negative(b)) => Term::Negative(a.union(b)),
        }
    }

    pub fn union(&self, other: &Term) -> Term {
        match (self, other) {
            (Term::Positive(a), Term::Positive(b)) => Term::Positive(a.union(b)),
            (Term::Positive(p), Term::Negative(n)) | (Term::Negative(n), Term::Positive(p)) => {
                Term::Negative(p.complement().intersection(n))
            }
            (Term::Negative(a), Term::Negative(b)) => Term::Negative(a.intersection(b)),
        }
    }

    pub fn is_disjoint(&self, other: &Term) -> bool {
        match (self, other) {
            (Term::Positive(a), Term::Positive(b)) => a.intersection(b) == Ranges::empty(),
            (Term::Positive(p), Term::Negative(n)) | (Term::Negative(n), Term::Positive(p)) => {
                p.subset_of(n)
            }
            (Term::Negative(_), Term::Negative(_)) => false,
        }
    }

    fn subset_of(&self, other: &Term) -> bool {
        match (self, other) {
            (Term::Positive(a), Term::Positive(b)) => a.subset_of(b),
            (Term::Positive(p), Term::Negative(n)) => p.intersection(n) == Ranges::empty(),
            (Term::Negative(_), Term::Positive(_)) => false,
            (Term::Negative(a), Term::Negative(b)) => b.subset_of(a),
        }
    }

    pub fn relation_with(&self, other: &Term) -> Relation {
        if self.subset_of(other) {
            Relation::Satisfied
        } else if self.is_disjoint(other) {
            Relation::Contradicted
        } else {
            Relation::Inconclusive
        }
    }

    fn is_any(&self) -> bool {
        match self {
            Term::Negative(r) => *r == Ranges::empty(),
            Term::Positive(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Incompatibility
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Incompatibility {
    pub package_terms: BTreeMap<PackageId, Term>,
    pub kind: IncompatKind,
}

#[derive(Debug, Clone)]
pub enum IncompatKind {
    Root,
    NoVersions(PackageId, VS),
    Dependency(PackageId, PackageId),
    Derived(IncompatId, IncompatId),
}

impl Incompatibility {
    pub fn not_root(root: PackageId, version: VypVersion) -> Self {
        let mut terms = BTreeMap::new();
        terms.insert(root, Term::Negative(Ranges::singleton(version)));
        Incompatibility {
            package_terms: terms,
            kind: IncompatKind::Root,
        }
    }

    pub fn no_versions(pkg: PackageId, range: VS) -> Self {
        let mut terms = BTreeMap::new();
        terms.insert(pkg, Term::Positive(range.clone()));
        Incompatibility {
            package_terms: terms,
            kind: IncompatKind::NoVersions(pkg, range),
        }
    }

    pub fn from_dependency(
        pkg: PackageId,
        version: &VypVersion,
        dep: PackageId,
        dep_range: &VS,
    ) -> Self {
        let mut terms = BTreeMap::new();
        terms.insert(pkg, Term::Positive(Ranges::singleton(version.clone())));
        terms.insert(dep, Term::Negative(dep_range.clone()));
        Incompatibility {
            package_terms: terms,
            kind: IncompatKind::Dependency(pkg, dep),
        }
    }

    pub fn is_terminal(&self, root: PackageId) -> bool {
        if self.package_terms.is_empty() {
            return true;
        }
        if self.package_terms.len() == 1 {
            return self.package_terms.contains_key(&root);
        }
        false
    }

    pub fn prior_cause(
        a: &Incompatibility,
        b: &Incompatibility,
        pivot: PackageId,
    ) -> Incompatibility {
        let mut terms: BTreeMap<PackageId, Term> = BTreeMap::new();

        for (&pkg, term) in &a.package_terms {
            if pkg != pivot {
                terms.insert(pkg, term.clone());
            }
        }

        for (&pkg, term) in &b.package_terms {
            if pkg != pivot {
                if let Some(existing) = terms.get(&pkg) {
                    let merged = existing.intersection(term);
                    terms.insert(pkg, merged);
                } else {
                    terms.insert(pkg, term.clone());
                }
            }
        }

        let pivot_a = a.package_terms.get(&pivot);
        let pivot_b = b.package_terms.get(&pivot);
        let pivot_term = match (pivot_a, pivot_b) {
            (Some(ta), Some(tb)) => Some(ta.union(tb)),
            (Some(t), None) | (None, Some(t)) => Some(t.clone()),
            (None, None) => None,
        };
        if let Some(t) = pivot_term {
            if !t.is_any() {
                terms.insert(pivot, t);
            }
        }

        Incompatibility {
            package_terms: terms,
            kind: IncompatKind::Derived(IncompatId(0), IncompatId(0)),
        }
    }
}

// ---------------------------------------------------------------------------
// PartialSolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct DecisionLevel(pub u32);

#[derive(Debug, Clone)]
struct DatedDerivation {
    global_index: u32,
    decision_level: u32,
    cause: IncompatId,
    accumulated_intersection: Term,
}

#[derive(Debug, Clone)]
enum AssignmentState {
    Decision(u32, VypVersion, u32),
    Derivations(Term),
}

#[derive(Debug, Clone)]
struct PackageAssignment {
    state: AssignmentState,
    derivations: Vec<DatedDerivation>,
    smallest_dl: u32,
    highest_dl: u32,
}

pub struct PartialSolution {
    pub decision_level: u32,
    next_global_index: u32,
    assignments: IndexMap<PackageId, PackageAssignment>,
}

impl Default for PartialSolution {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialSolution {
    pub fn new() -> Self {
        Self {
            decision_level: 0,
            next_global_index: 0,
            assignments: IndexMap::new(),
        }
    }

    pub fn add_decision(&mut self, pkg: PackageId, version: VypVersion) {
        self.decision_level += 1;
        let dl = self.decision_level;
        let gi = self.next_global_index;
        self.next_global_index += 1;

        if let Some(pa) = self.assignments.get_mut(&pkg) {
            pa.state = AssignmentState::Decision(dl, version, gi);
            pa.highest_dl = dl;
        } else {
            self.assignments.insert(
                pkg,
                PackageAssignment {
                    state: AssignmentState::Decision(dl, version, gi),
                    derivations: Vec::new(),
                    smallest_dl: dl,
                    highest_dl: dl,
                },
            );
        }
    }

    pub fn add_derivation(&mut self, pkg: PackageId, cause: IncompatId, new_term: Term) {
        let dl = self.decision_level;
        let gi = self.next_global_index;
        self.next_global_index += 1;

        let pa = self
            .assignments
            .entry(pkg)
            .or_insert_with(|| PackageAssignment {
                state: AssignmentState::Derivations(Term::any()),
                derivations: Vec::new(),
                smallest_dl: dl,
                highest_dl: dl,
            });

        let accumulated = match &pa.state {
            AssignmentState::Derivations(current) => current.intersection(&new_term),
            AssignmentState::Decision(_, _, _) => return,
        };

        pa.derivations.push(DatedDerivation {
            global_index: gi,
            decision_level: dl,
            cause,
            accumulated_intersection: accumulated.clone(),
        });
        pa.state = AssignmentState::Derivations(accumulated);
        pa.highest_dl = dl;
    }

    pub fn term_for(&self, pkg: PackageId) -> Term {
        match self.assignments.get(&pkg) {
            Some(pa) => match &pa.state {
                AssignmentState::Decision(_, v, _) => Term::Positive(Ranges::singleton(v.clone())),
                AssignmentState::Derivations(t) => t.clone(),
            },
            None => Term::any(),
        }
    }

    pub fn is_decided(&self, pkg: PackageId) -> bool {
        self.assignments
            .get(&pkg)
            .map(|pa| matches!(pa.state, AssignmentState::Decision(_, _, _)))
            .unwrap_or(false)
    }

    /// Whether `pkg` currently has any assignment (decision or derivation).
    pub fn is_assigned(&self, pkg: PackageId) -> bool {
        self.assignments.contains_key(&pkg)
    }

    pub fn backtrack(&mut self, target_dl: u32) {
        self.decision_level = target_dl;
        let mut to_remove = Vec::new();

        for (&pkg, pa) in self.assignments.iter_mut() {
            if pa.smallest_dl > target_dl {
                to_remove.push(pkg);
                continue;
            }
            if pa.highest_dl <= target_dl {
                continue;
            }
            while pa
                .derivations
                .last()
                .is_some_and(|d| d.decision_level > target_dl)
            {
                pa.derivations.pop();
            }
            if let Some(last) = pa.derivations.last() {
                pa.state = AssignmentState::Derivations(last.accumulated_intersection.clone());
                pa.highest_dl = last.decision_level;
            } else {
                pa.state = AssignmentState::Derivations(Term::any());
                pa.highest_dl = pa.smallest_dl;
            }
        }

        for pkg in to_remove {
            self.assignments.swap_remove(&pkg);
        }
    }

    pub fn undecided_packages(&self) -> Vec<(PackageId, VS)> {
        let mut result = Vec::new();
        self.fill_undecided(&mut result);
        result
    }

    pub fn fill_undecided(&self, buf: &mut Vec<(PackageId, VS)>) {
        buf.clear();
        for (&pkg, pa) in &self.assignments {
            if let AssignmentState::Derivations(Term::Positive(range)) = &pa.state {
                buf.push((pkg, range.clone()));
            }
        }
    }

    pub fn extract_solution(&self) -> HashMap<PackageId, VypVersion> {
        let mut solution = HashMap::new();
        for (&pkg, pa) in &self.assignments {
            if let AssignmentState::Decision(_, ref v, _) = pa.state {
                solution.insert(pkg, v.clone());
            }
        }
        solution
    }

    pub fn satisfier_search(&self, incompat: &Incompatibility) -> SatisfierInfo {
        let mut satisfier_pkg = PackageId(0);
        let mut satisfier_gi: u32 = 0;
        let mut satisfier_dl: u32 = 0;
        let mut satisfier_cause: Option<IncompatId> = None;
        let mut satisfier_is_decision = false;

        for (&pkg, incompat_term) in &incompat.package_terms {
            let pa = match self.assignments.get(&pkg) {
                Some(pa) => pa,
                None => continue,
            };

            let negate = incompat_term.negate();
            let info = self.find_satisfier(pa, &negate);

            if info.global_index >= satisfier_gi {
                satisfier_gi = info.global_index;
                satisfier_dl = info.decision_level;
                satisfier_pkg = pkg;
                satisfier_cause = info.cause;
                satisfier_is_decision = info.is_decision;
            }
        }

        let mut prev_dl: u32 = 0;
        for (&pkg, incompat_term) in &incompat.package_terms {
            if pkg == satisfier_pkg {
                continue;
            }
            let pa = match self.assignments.get(&pkg) {
                Some(pa) => pa,
                None => continue,
            };
            let negate = incompat_term.negate();
            let info = self.find_satisfier(pa, &negate);
            if info.decision_level > prev_dl {
                prev_dl = info.decision_level;
            }
        }

        SatisfierInfo {
            package: satisfier_pkg,
            decision_level: satisfier_dl,
            previous_dl: prev_dl,
            cause: satisfier_cause,
            is_decision: satisfier_is_decision,
        }
    }

    fn find_satisfier(&self, pa: &PackageAssignment, start_term: &Term) -> FindSatisfierResult {
        for dd in &pa.derivations {
            if dd.accumulated_intersection.is_disjoint(start_term) {
                return FindSatisfierResult {
                    global_index: dd.global_index,
                    decision_level: dd.decision_level,
                    cause: Some(dd.cause),
                    is_decision: false,
                };
            }
        }
        match &pa.state {
            AssignmentState::Decision(dl, _, gi) => FindSatisfierResult {
                global_index: *gi,
                decision_level: *dl,
                cause: None,
                is_decision: true,
            },
            _ => FindSatisfierResult {
                global_index: self.next_global_index,
                decision_level: pa.highest_dl,
                cause: None,
                is_decision: false,
            },
        }
    }
}

struct FindSatisfierResult {
    global_index: u32,
    decision_level: u32,
    cause: Option<IncompatId>,
    is_decision: bool,
}

pub struct SatisfierInfo {
    pub package: PackageId,
    pub decision_level: u32,
    pub previous_dl: u32,
    pub cause: Option<IncompatId>,
    pub is_decision: bool,
}

// ---------------------------------------------------------------------------
// VSIDS scoring
// ---------------------------------------------------------------------------

/// When any VSIDS score (or the bump increment) exceeds this, all scores are
/// rescaled down. Matches the standard SAT-solver activity-rescale guard.
const VSIDS_RESCALE_THRESHOLD: f64 = 1e100;

pub struct VsidsScoring {
    scores: HashMap<PackageId, f64>,
    bump_increment: f64,
    decay_factor: f64,
}

impl Default for VsidsScoring {
    fn default() -> Self {
        Self::new()
    }
}

impl VsidsScoring {
    pub fn new() -> Self {
        Self {
            scores: HashMap::new(),
            bump_increment: 1.0,
            decay_factor: 0.95,
        }
    }

    pub fn bump(&mut self, pkg: PackageId) {
        let score = self.scores.entry(pkg).or_default();
        *score += self.bump_increment;
        // Rescale all scores down if any grows too large, so neither the scores
        // nor the (geometrically growing) bump increment can overflow to `inf`
        // on pathologically long resolves. This preserves relative ordering.
        if *score > VSIDS_RESCALE_THRESHOLD {
            self.rescale();
        }
    }

    pub fn decay(&mut self) {
        self.bump_increment /= self.decay_factor;
        if self.bump_increment > VSIDS_RESCALE_THRESHOLD {
            self.rescale();
        }
    }

    /// Divide every score and the bump increment by the rescale threshold,
    /// keeping all values finite while preserving their relative order.
    fn rescale(&mut self) {
        let inv = 1.0 / VSIDS_RESCALE_THRESHOLD;
        for v in self.scores.values_mut() {
            *v *= inv;
        }
        self.bump_increment *= inv;
        if self.bump_increment == 0.0 {
            self.bump_increment = 1.0;
        }
    }

    pub fn score(&self, pkg: PackageId) -> f64 {
        self.scores.get(&pkg).copied().unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// SolverState
// ---------------------------------------------------------------------------

pub struct SolverState {
    pub root_package: PackageId,
    pub root_version: VypVersion,
    /// Two-watched-literals index: each incompatibility appears in the watch
    /// list of at most two of its packages. An incompatibility is only
    /// re-examined when one of its two watched packages is assigned, instead
    /// of being rescanned on every change to any of its packages.
    watches: HashMap<PackageId, Vec<IncompatId>>,
    /// The two packages each incompatibility currently watches, indexed by
    /// `IncompatId`. For single-term incompatibilities both entries are equal.
    watched: Vec<(PackageId, PackageId)>,
    store: Vec<Incompatibility>,
    /// Derived (learned) incompatibilities still present in the watch index,
    /// in registration order, for bounded garbage collection.
    learned_watched: std::collections::VecDeque<IncompatId>,
    pub partial_solution: PartialSolution,
    pub vsids: VsidsScoring,

    pkg_to_id: HashMap<VypPackage, PackageId>,
    id_to_pkg: Vec<VypPackage>,
    next_pkg_id: u32,

    added_deps: HashMap<(PackageId, VypVersion), bool>,
}

/// Cap on learned incompatibilities kept in the watch index. Beyond this we
/// stop watching the oldest learned clauses (they remain in `store` for
/// conflict-cause analysis). Forgetting learned clauses is always
/// correctness-preserving — they are entailed by the original incompatibilities
/// — and only trades a possible re-derivation for bounded propagation cost.
const MAX_LEARNED_WATCHED: usize = 10_000;

#[derive(Debug)]
pub enum SolverError {
    NoSolution {
        derivation_tree: String,
        contested_packages: Vec<String>,
    },
    Cancelled,
}

impl fmt::Display for SolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SolverError::NoSolution {
                derivation_tree, ..
            } => write!(f, "{}", derivation_tree),
            SolverError::Cancelled => write!(f, "resolution cancelled"),
        }
    }
}

impl SolverState {
    pub fn new(root: VypPackage, root_version: VypVersion) -> Self {
        let mut state = Self {
            root_package: PackageId(0),
            root_version: root_version.clone(),
            watches: HashMap::new(),
            watched: Vec::new(),
            store: Vec::new(),
            learned_watched: std::collections::VecDeque::new(),
            partial_solution: PartialSolution::new(),
            vsids: VsidsScoring::new(),
            pkg_to_id: HashMap::new(),
            id_to_pkg: Vec::new(),
            next_pkg_id: 0,
            added_deps: HashMap::new(),
        };

        let root_id = state.get_or_create_package(&root);
        state.root_package = root_id;

        let not_root = Incompatibility::not_root(root_id, root_version.clone());
        state.register_incompatibility(not_root);

        // PubGrub requires the root decision to be made before propagation
        state.partial_solution.add_decision(root_id, root_version);

        state
    }

    pub fn get_or_create_package(&mut self, pkg: &VypPackage) -> PackageId {
        if let Some(&id) = self.pkg_to_id.get(pkg) {
            return id;
        }
        let id = PackageId(self.next_pkg_id);
        self.next_pkg_id += 1;
        self.pkg_to_id.insert(pkg.clone(), id);
        self.id_to_pkg.push(pkg.clone());
        id
    }

    pub fn package_name(&self, id: PackageId) -> &VypPackage {
        &self.id_to_pkg[id.0 as usize]
    }

    fn register_incompatibility(&mut self, incompat: Incompatibility) -> IncompatId {
        let id = IncompatId(self.store.len() as u32);
        let (a, b) = self.pick_initial_watches(&incompat);
        self.watched.push((a, b));
        self.add_watch(a, id);
        if b != a {
            self.add_watch(b, id);
        }
        let is_derived = matches!(incompat.kind, IncompatKind::Derived(_, _));
        self.store.push(incompat);
        if is_derived {
            self.learned_watched.push_back(id);
            self.gc_learned_watches();
        }
        id
    }

    /// Choose two packages to watch, preferring terms not currently satisfied
    /// by the partial solution. This establishes the watch invariant: if both
    /// watched terms are unsatisfied, the incompatibility can be neither a
    /// conflict (all terms satisfied) nor unit (all-but-one satisfied).
    fn pick_initial_watches(&self, incompat: &Incompatibility) -> (PackageId, PackageId) {
        let mut unsatisfied = Vec::new();
        let mut any = None;
        for (&pkg, term) in &incompat.package_terms {
            any.get_or_insert(pkg);
            if self.partial_solution.term_for(pkg).relation_with(term) != Relation::Satisfied {
                unsatisfied.push(pkg);
                if unsatisfied.len() == 2 {
                    break;
                }
            }
        }
        match unsatisfied.len() {
            2 => (unsatisfied[0], unsatisfied[1]),
            1 => {
                // One unsatisfied term; pick any second distinct package.
                let first = unsatisfied[0];
                let second = incompat
                    .package_terms
                    .keys()
                    .copied()
                    .find(|&p| p != first)
                    .unwrap_or(first);
                (first, second)
            }
            _ => {
                let p = any.expect("incompatibility has at least one term");
                (p, p)
            }
        }
    }

    fn add_watch(&mut self, pkg: PackageId, id: IncompatId) {
        self.watches.entry(pkg).or_default().push(id);
    }

    fn remove_watch(&mut self, pkg: PackageId, id: IncompatId) {
        if let Some(list) = self.watches.get_mut(&pkg) {
            if let Some(pos) = list.iter().position(|&x| x == id) {
                list.swap_remove(pos);
            }
        }
    }

    /// Re-point an incompatibility's two watches at `(a, b)`, updating the
    /// per-package watch lists.
    fn set_watches(&mut self, id: IncompatId, a: PackageId, b: PackageId) {
        let (old_a, old_b) = self.watched[id.0 as usize];
        if (old_a, old_b) == (a, b) || (old_a, old_b) == (b, a) {
            return;
        }
        self.remove_watch(old_a, id);
        if old_b != old_a {
            self.remove_watch(old_b, id);
        }
        self.add_watch(a, id);
        if b != a {
            self.add_watch(b, id);
        }
        self.watched[id.0 as usize] = (a, b);
    }

    /// Maintain the watch invariant for a dormant/inconclusive incompatibility
    /// by pointing both watches at currently-unsatisfied terms.
    fn refresh_watches(&mut self, id: IncompatId) {
        let incompat = &self.store[id.0 as usize];
        let mut unsatisfied = Vec::new();
        for (&pkg, term) in &incompat.package_terms {
            if self.partial_solution.term_for(pkg).relation_with(term) != Relation::Satisfied {
                unsatisfied.push(pkg);
                if unsatisfied.len() == 2 {
                    break;
                }
            }
        }
        if unsatisfied.len() == 2 {
            self.set_watches(id, unsatisfied[0], unsatisfied[1]);
        }
    }

    /// Ensure `pkg` is one of the two packages watching `id` (so a later change
    /// to `pkg` — e.g. on backtrack — re-examines this incompatibility).
    fn ensure_watched(&mut self, id: IncompatId, pkg: PackageId) {
        let (a, b) = self.watched[id.0 as usize];
        if a == pkg || b == pkg {
            return;
        }
        self.set_watches(id, pkg, a);
    }

    /// Forget the oldest learned incompatibilities once the watch index grows
    /// past [`MAX_LEARNED_WATCHED`]. They stay in `store` (so conflict-cause
    /// analysis remains valid); only their watch entries are dropped.
    fn gc_learned_watches(&mut self) {
        while self.learned_watched.len() > MAX_LEARNED_WATCHED {
            let Some(id) = self.learned_watched.pop_front() else {
                break;
            };
            let (a, b) = self.watched[id.0 as usize];
            self.remove_watch(a, id);
            if b != a {
                self.remove_watch(b, id);
            }
        }
    }

    pub fn add_incompatibility_for_no_versions(&mut self, pkg: PackageId, range: VS) -> IncompatId {
        let incompat = Incompatibility::no_versions(pkg, range);
        self.register_incompatibility(incompat)
    }

    pub fn add_dependency_incompatibility(
        &mut self,
        pkg: PackageId,
        version: &VypVersion,
        dep: PackageId,
        dep_range: &VS,
    ) -> IncompatId {
        let incompat = Incompatibility::from_dependency(pkg, version, dep, dep_range);
        self.register_incompatibility(incompat)
    }

    pub fn mark_deps_added(&mut self, pkg: PackageId, version: &VypVersion) {
        self.added_deps.insert((pkg, version.clone()), true);
    }

    pub fn add_dependencies(
        &mut self,
        pkg: PackageId,
        version: &VypVersion,
        deps: &[(PackageId, VS)],
    ) -> bool {
        let key = (pkg, version.clone());
        if self.added_deps.contains_key(&key) {
            self.partial_solution.add_decision(pkg, version.clone());
            return false;
        }
        self.added_deps.insert(key, true);

        for (dep, range) in deps {
            self.add_dependency_incompatibility(pkg, version, *dep, range);
        }

        self.partial_solution.add_decision(pkg, version.clone());
        false
    }

    // -----------------------------------------------------------------------
    // Unit propagation
    // -----------------------------------------------------------------------

    pub fn unit_propagation(
        &mut self,
        start: PackageId,
    ) -> Result<Vec<(PackageId, IncompatId)>, SolverError> {
        let mut satisfier_causes = Vec::new();
        let mut buffer = vec![start];
        let mut buffer_set = HashSet::new();
        buffer_set.insert(start);

        while let Some(current) = buffer.pop() {
            buffer_set.remove(&current);

            // Examine only incompatibilities watching `current`. Snapshot the
            // list because moving watches mutates it during iteration.
            let watchers: Vec<IncompatId> = self.watches.get(&current).cloned().unwrap_or_default();

            let mut conflict_id = None;

            for iid in watchers {
                // The watch may have already moved off `current`.
                let (wa, wb) = self.watched[iid.0 as usize];
                if wa != current && wb != current {
                    continue;
                }

                match self.check_relation(&self.store[iid.0 as usize]) {
                    IncompatRelation::Satisfied => {
                        conflict_id = Some(iid);
                        break;
                    }
                    IncompatRelation::AlmostSatisfied(pkg_almost) => {
                        let negate = self.store[iid.0 as usize]
                            .package_terms
                            .get(&pkg_almost)
                            .unwrap()
                            .negate();
                        self.partial_solution
                            .add_derivation(pkg_almost, iid, negate);
                        // Keep watching the asserted package so a later
                        // backtrack re-examines this incompatibility.
                        self.ensure_watched(iid, pkg_almost);
                        if buffer_set.insert(pkg_almost) {
                            buffer.push(pkg_almost);
                        }
                    }
                    IncompatRelation::Contradicted | IncompatRelation::Inconclusive => {
                        // Still has >= 2 unsatisfied terms: keep both watches on
                        // unsatisfied terms to preserve the watch invariant.
                        self.refresh_watches(iid);
                    }
                }
            }

            if let Some(iid) = conflict_id {
                match self.conflict_resolution(iid, &mut satisfier_causes)? {
                    ConflictOutcome::Backtracked {
                        package,
                        root_cause,
                    } => {
                        buffer.clear();
                        buffer_set.clear();
                        buffer.push(package);
                        buffer_set.insert(package);
                        let negate = self.store[root_cause.0 as usize]
                            .package_terms
                            .get(&package)
                            .unwrap()
                            .negate();
                        self.partial_solution
                            .add_derivation(package, root_cause, negate);
                        self.ensure_watched(root_cause, package);

                        // A backtrack to decision level 0 removes the root
                        // decision (originally made at level 1). Re-establish it
                        // and re-queue it so its top-level dependency
                        // incompatibilities re-propagate; otherwise the solver
                        // forgets the project's own requirements and returns an
                        // empty (incorrect) solution instead of reporting that no
                        // solution exists.
                        let root = self.root_package;
                        if !self.partial_solution.is_assigned(root) {
                            let root_version = self.root_version.clone();
                            self.partial_solution.add_decision(root, root_version);
                            if buffer_set.insert(root) {
                                buffer.push(root);
                            }
                        }
                    }
                }
            }
        }

        Ok(satisfier_causes)
    }

    fn check_relation(&self, incompat: &Incompatibility) -> IncompatRelation {
        let mut all_satisfied = true;
        let mut almost_pkg = None;

        for (&pkg, term) in &incompat.package_terms {
            let sol_term = self.partial_solution.term_for(pkg);
            match sol_term.relation_with(term) {
                Relation::Satisfied => {}
                Relation::Contradicted => return IncompatRelation::Contradicted,
                Relation::Inconclusive => {
                    if !all_satisfied {
                        return IncompatRelation::Inconclusive;
                    }
                    all_satisfied = false;
                    almost_pkg = Some(pkg);
                }
            }
        }

        if all_satisfied {
            IncompatRelation::Satisfied
        } else {
            IncompatRelation::AlmostSatisfied(almost_pkg.unwrap())
        }
    }

    // -----------------------------------------------------------------------
    // Conflict resolution
    // -----------------------------------------------------------------------

    fn prior_cause_from_store(
        &self,
        a: IncompatId,
        b: IncompatId,
        pivot: PackageId,
    ) -> Incompatibility {
        let mut prior = Incompatibility::prior_cause(
            &self.store[a.0 as usize],
            &self.store[b.0 as usize],
            pivot,
        );
        prior.kind = IncompatKind::Derived(a, b);
        prior
    }

    fn conflict_resolution(
        &mut self,
        incompat_id: IncompatId,
        satisfier_causes: &mut Vec<(PackageId, IncompatId)>,
    ) -> Result<ConflictOutcome, SolverError> {
        let mut current_id = incompat_id;

        loop {
            let current = &self.store[current_id.0 as usize];

            if current.is_terminal(self.root_package) {
                let report = self.build_error_report(current_id);
                let contested = self.extract_contested_packages(current_id);
                return Err(SolverError::NoSolution {
                    derivation_tree: report,
                    contested_packages: contested,
                });
            }

            let info = self.partial_solution.satisfier_search(current);

            if !info.is_decision {
                if let Some(cause) = info.cause {
                    let prior = self.prior_cause_from_store(current_id, cause, info.package);
                    current_id = self.register_incompatibility(prior);
                    satisfier_causes.push((info.package, current_id));
                    self.vsids.bump(info.package);
                    self.vsids.decay();
                    continue;
                }
            }

            // Satisfier is a decision — check DifferentLevels vs SameLevel
            if info.previous_dl < info.decision_level {
                self.partial_solution.backtrack(info.previous_dl);
                satisfier_causes.push((info.package, current_id));
                self.vsids.bump(info.package);
                return Ok(ConflictOutcome::Backtracked {
                    package: info.package,
                    root_cause: current_id,
                });
            } else if let Some(cause) = info.cause {
                let prior = self.prior_cause_from_store(current_id, cause, info.package);
                current_id = self.register_incompatibility(prior);
                satisfier_causes.push((info.package, current_id));
                self.vsids.bump(info.package);
                self.vsids.decay();
            } else {
                let report = self.build_error_report(current_id);
                let contested = self.extract_contested_packages(current_id);
                return Err(SolverError::NoSolution {
                    derivation_tree: report,
                    contested_packages: contested,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pick next package — combines VSIDS with version-count heuristic
    // -----------------------------------------------------------------------

    pub fn pick_next_package(
        &self,
        version_counts: &HashMap<PackageId, usize>,
    ) -> Option<(PackageId, VS)> {
        let undecided = self.partial_solution.undecided_packages();
        if undecided.is_empty() {
            return None;
        }

        let mut best: Option<(PackageId, VS, f64)> = None;
        for (pkg, range) in undecided {
            let count = version_counts.get(&pkg).copied().unwrap_or(usize::MAX);
            let base_priority = if count == 0 {
                f64::MAX
            } else {
                1.0 / (count as f64)
            };
            let vsids_score = self.vsids.score(pkg);
            let priority = base_priority + vsids_score;

            if best.as_ref().is_none_or(|(_, _, bp)| priority > *bp) {
                best = Some((pkg, range, priority));
            }
        }

        best.map(|(pkg, range, _)| (pkg, range))
    }

    // -----------------------------------------------------------------------
    // Error reporting — walk derivation tree
    // -----------------------------------------------------------------------

    fn extract_contested_packages(&self, terminal: IncompatId) -> Vec<String> {
        let mut packages = Vec::new();
        self.collect_contested(terminal, &mut packages);
        packages.sort();
        packages.dedup();
        packages
    }

    fn collect_contested(&self, id: IncompatId, packages: &mut Vec<String>) {
        let incompat = &self.store[id.0 as usize];
        match &incompat.kind {
            IncompatKind::Dependency(from, to) => {
                let from_name = self.package_name(*from);
                let to_name = self.package_name(*to);
                if !from_name.is_root() {
                    packages.push(from_name.name().to_string());
                }
                if !to_name.is_root() {
                    packages.push(to_name.name().to_string());
                }
            }
            IncompatKind::Derived(a, b) => {
                self.collect_contested(*a, packages);
                self.collect_contested(*b, packages);
            }
            IncompatKind::NoVersions(pkg, _) => {
                let name = self.package_name(*pkg);
                if !name.is_root() {
                    packages.push(name.name().to_string());
                }
            }
            IncompatKind::Root => {}
        }
    }

    fn build_error_report(&self, terminal: IncompatId) -> String {
        let mut lines = Vec::new();
        self.explain_incompat(terminal, &mut lines, 0);
        lines.join("\n")
    }

    fn explain_incompat(&self, id: IncompatId, lines: &mut Vec<String>, depth: usize) {
        let incompat = &self.store[id.0 as usize];
        let indent = "  ".repeat(depth);

        match &incompat.kind {
            IncompatKind::Root => {
                lines.push(format!("{}the project requires these dependencies", indent));
            }
            IncompatKind::NoVersions(pkg, range) => {
                let name = self.package_name(*pkg);
                lines.push(format!(
                    "{}no versions of {} match {}",
                    indent,
                    name,
                    format_range(range)
                ));
            }
            IncompatKind::Dependency(from, to) => {
                let from_name = self.package_name(*from);
                let to_name = self.package_name(*to);
                let from_term = incompat.package_terms.get(from);
                let to_term = incompat.package_terms.get(to);
                let from_range = from_term.map(format_term).unwrap_or_default();
                let to_range = to_term.map(format_term).unwrap_or_default();
                lines.push(format!(
                    "{}{} {} depends on {} {}",
                    indent, from_name, from_range, to_name, to_range
                ));
            }
            IncompatKind::Derived(a, b) => {
                self.explain_incompat(*a, lines, depth + 1);
                self.explain_incompat(*b, lines, depth + 1);
                let terms_desc: Vec<String> = incompat
                    .package_terms
                    .iter()
                    .map(|(pkg, term)| format!("{} {}", self.package_name(*pkg), format_term(term)))
                    .collect();
                lines.push(format!("{}So, {}", indent, terms_desc.join(" and ")));
            }
        }
    }
}

#[derive(Debug)]
enum IncompatRelation {
    Satisfied,
    AlmostSatisfied(PackageId),
    Contradicted,
    Inconclusive,
}

enum ConflictOutcome {
    Backtracked {
        package: PackageId,
        root_cause: IncompatId,
    },
}

fn format_range(range: &VS) -> String {
    format!("{}", range)
}

fn format_term(term: &Term) -> String {
    match term {
        Term::Positive(r) => format_range(r),
        Term::Negative(r) => format!("not {}", format_range(r)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[u32]) -> VypVersion {
        VypVersion::new(parts.to_vec())
    }

    #[test]
    fn term_positive_intersection() {
        let a = Term::Positive(Ranges::higher_than(v(&[1, 0, 0])));
        let b = Term::Positive(Ranges::strictly_lower_than(v(&[2, 0, 0])));
        let c = a.intersection(&b);
        assert!(matches!(c, Term::Positive(_)));
        if let Term::Positive(r) = c {
            assert!(r.contains(&v(&[1, 5, 0])));
            assert!(!r.contains(&v(&[2, 0, 0])));
            assert!(!r.contains(&v(&[0, 9, 0])));
        }
    }

    #[test]
    fn term_relation() {
        let sol = Term::Positive(Ranges::singleton(v(&[1, 0, 0])));
        let incompat = Term::Positive(Ranges::higher_than(v(&[1, 0, 0])));
        assert_eq!(sol.relation_with(&incompat), Relation::Satisfied);
    }

    #[test]
    fn incompatibility_terminal() {
        let root = PackageId(0);
        let i = Incompatibility::not_root(root, v(&[0]));
        assert!(i.is_terminal(root));

        let other = PackageId(1);
        let i2 = Incompatibility::no_versions(other, Ranges::full());
        assert!(!i2.is_terminal(root));
    }

    #[test]
    fn partial_solution_decision_and_backtrack() {
        let mut ps = PartialSolution::new();
        let pkg = PackageId(1);
        ps.add_decision(pkg, v(&[1, 0, 0]));
        assert!(ps.is_decided(pkg));
        assert_eq!(ps.decision_level, 1);

        ps.backtrack(0);
        assert!(!ps.is_decided(pkg));
        assert_eq!(ps.decision_level, 0);
    }

    #[test]
    fn vsids_scoring() {
        let mut vs = VsidsScoring::new();
        let a = PackageId(0);
        let b = PackageId(1);

        vs.bump(a);
        vs.bump(a);
        vs.bump(b);
        assert!(vs.score(a) > vs.score(b));

        vs.decay();
        vs.bump(b);
        // After decay, b's latest bump is worth more
        // b total = 1.0 + (1.0/0.95) ≈ 2.05
        // a total = 2.0
        assert!(vs.score(b) > vs.score(a));
    }

    #[test]
    fn solver_simple_resolution() {
        let root = VypPackage::Root;
        let root_v = v(&[0]);
        let mut state = SolverState::new(root.clone(), root_v.clone());

        let a_pkg = VypPackage::named("a");
        let a_id = state.get_or_create_package(&a_pkg);

        // Root depends on a >= 1.0
        let dep_range = Ranges::higher_than(v(&[1, 0, 0]));
        let root_id = state.root_package;
        state.add_dependency_incompatibility(root_id, &root_v, a_id, &dep_range);

        // Root is already decided by SolverState::new. Unit propagation
        // should derive a = Positive(>=1.0.0) from the dependency incompat.
        let result = state.unit_propagation(root_id);
        assert!(result.is_ok());

        let undecided = state.partial_solution.undecided_packages();
        assert_eq!(undecided.len(), 1);
        assert_eq!(undecided[0].0, a_id);
    }
}
