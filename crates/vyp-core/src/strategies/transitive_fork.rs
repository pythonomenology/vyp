use vyp_api::traits::conflict_strategy::{
    ConflictContext, ConflictStrategy, ConflictSuggestion, ForkSpec, StrategyVerdict,
};

/// Built-in strategy that handles transitive conflict declarations
/// by forking resolution for the contested package.
#[derive(Debug)]
pub struct TransitiveForkStrategy;

impl ConflictStrategy for TransitiveForkStrategy {
    fn name(&self) -> &str {
        "transitive-fork"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn evaluate(&self, context: &ConflictContext) -> StrategyVerdict {
        // Only act if there are inherited transitive conflicts for the contested package
        let relevant_conflicts: Vec<_> = context
            .inherited_conflicts
            .iter()
            .filter(|c| c.transitive && c.on.iter().any(|p| p == context.contested_package.name()))
            .collect();

        if relevant_conflicts.is_empty() {
            return StrategyVerdict::Abstain;
        }

        if let Some(conflict) = relevant_conflicts.into_iter().next() {
            let forks: Vec<ForkSpec> = conflict
                .sides
                .iter()
                .map(|side| ForkSpec {
                    side: side.clone(),
                    lower: None,
                    upper: None,
                    upper_inclusive: false,
                })
                .collect();

            StrategyVerdict::Fork {
                conflict_name: conflict.name.clone(),
                forks,
            }
        } else {
            StrategyVerdict::Abstain
        }
    }

    fn suggest(&self, context: &ConflictContext) -> Vec<ConflictSuggestion> {
        let has_inherited = context
            .inherited_conflicts
            .iter()
            .any(|c| c.transitive && c.on.iter().any(|p| p == context.contested_package.name()));

        if has_inherited {
            return Vec::new();
        }

        vec![ConflictSuggestion {
            source: self.name().to_string(),
            message: format!(
                "Consider declaring a transitive conflict on '{}'",
                context.contested_package.name()
            ),
            command: Some(format!(
                "vyp conflict add --on {} --transitive",
                context.contested_package.name()
            )),
        }]
    }
}
