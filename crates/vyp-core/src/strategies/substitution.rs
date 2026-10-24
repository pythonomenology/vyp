use vyp_api::traits::conflict_strategy::{
    ConflictContext, ConflictStrategy, ConflictSuggestion, StrategyVerdict,
};
use vyp_api::traits::resolution_filter::{Candidate, ResolutionFilter};
use vyp_api::types::provenance::SelectionReason;
use vyp_api::types::substitution::SubstitutionSet;

/// Built-in strategy that handles package substitutions.
///
/// When a contested package is part of a substitution set, this strategy
/// ensures only one concrete provider is selected.
#[derive(Debug)]
pub struct SubstitutionStrategy {
    substitutions: Vec<SubstitutionSet>,
}

impl SubstitutionStrategy {
    pub fn new(substitutions: Vec<SubstitutionSet>) -> Self {
        Self { substitutions }
    }

    fn find_substitution(&self, package: &str) -> Option<&SubstitutionSet> {
        self.substitutions.iter().find(|s| s.contains(package))
    }
}

impl ConflictStrategy for SubstitutionStrategy {
    fn name(&self) -> &str {
        "substitution"
    }

    fn priority(&self) -> i32 {
        70
    }

    fn evaluate(&self, context: &ConflictContext) -> StrategyVerdict {
        let pkg_name = context.contested_package.name();

        if let Some(sub) = self.find_substitution(pkg_name) {
            // If the contested package is part of a substitution set,
            // we check if multiple substitutes are being pulled in
            let conflicting_substitutes: Vec<_> = context
                .requirements
                .iter()
                .filter(|(req_pkg, _)| sub.contains(req_pkg))
                .collect();

            if conflicting_substitutes.len() > 1 {
                return StrategyVerdict::Fail {
                    message: format!(
                        "Multiple packages from substitution set '{}' are required: {:?}. \
                         Only one can be installed. Use [tool.vyp.substitutions] to configure preference.",
                        sub.provides,
                        conflicting_substitutes
                            .iter()
                            .map(|(p, _)| p.as_str())
                            .collect::<Vec<_>>()
                    ),
                };
            }
        }

        StrategyVerdict::Abstain
    }

    fn suggest(&self, context: &ConflictContext) -> Vec<ConflictSuggestion> {
        let pkg_name = context.contested_package.name();
        if let Some(sub) = self.find_substitution(pkg_name) {
            let preferred = sub.prefer.as_deref().unwrap_or("(none)");
            return vec![ConflictSuggestion {
                source: self.name().to_string(),
                message: format!(
                    "Package '{}' is part of substitution set '{}' (preferred: {})",
                    pkg_name, sub.provides, preferred
                ),
                command: None,
            }];
        }
        Vec::new()
    }

    fn selection_reason(&self) -> SelectionReason {
        SelectionReason::Substitution
    }
}

/// Resolution filter that excludes substitute packages that aren't preferred.
#[derive(Debug)]
pub struct SubstitutionFilter {
    substitutions: Vec<SubstitutionSet>,
}

impl SubstitutionFilter {
    pub fn new(substitutions: Vec<SubstitutionSet>) -> Self {
        Self { substitutions }
    }
}

impl ResolutionFilter for SubstitutionFilter {
    fn name(&self) -> &str {
        "substitution-filter"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn filter(&self, candidates: &mut Vec<Candidate>) {
        for candidate in candidates.iter_mut() {
            let pkg_name = candidate.package.name().to_string();
            for sub in &self.substitutions {
                if sub.contains(&pkg_name) {
                    if let Some(preferred) = &sub.prefer {
                        if pkg_name != *preferred {
                            candidate.excluded = true;
                            candidate.exclusion_reason =
                                Some(format!("non-preferred substitute (prefer: {})", preferred));
                        }
                    }
                }
            }
        }
    }
}
