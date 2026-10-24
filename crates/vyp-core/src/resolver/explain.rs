use vyp_api::traits::conflict_strategy::{ConflictContext, ConflictSuggestion};

use crate::plugin::registry::StrategyRegistry;

/// Generates human-readable explanations when resolution fails.
pub fn explain_failure(
    context: &ConflictContext,
    strategy_registry: &StrategyRegistry,
) -> FailureExplanation {
    let mut suggestions = Vec::new();

    // Collect suggestions from all registered strategies
    for strategy in strategy_registry.strategies() {
        suggestions.extend(strategy.suggest(context));
    }

    // Add default suggestions
    if suggestions.is_empty() {
        for (pkg, _range) in &context.requirements {
            suggestions.push(ConflictSuggestion {
                source: "vyp".to_string(),
                message: format!(
                    "Check if {} has a newer version compatible with the required range",
                    pkg
                ),
                command: None,
            });
        }
    }

    suggestions.push(ConflictSuggestion {
        source: "vyp".to_string(),
        message: "Declare a transitive conflict to allow forked resolution".to_string(),
        command: Some(format!(
            "vyp conflict add --on {} --transitive",
            context.contested_package.name()
        )),
    });

    suggestions.push(ConflictSuggestion {
        source: "vyp".to_string(),
        message: "Override the version constraint".to_string(),
        command: Some(format!(
            "vyp override add {} \"<version_spec>\"",
            context.contested_package.name()
        )),
    });

    FailureExplanation {
        contested_package: context.contested_package.name().to_string(),
        requirements: context.requirements.clone(),
        suggestions,
    }
}

/// Structured explanation of a resolution failure.
#[derive(Debug)]
pub struct FailureExplanation {
    pub contested_package: String,
    pub requirements: Vec<(String, String)>,
    pub suggestions: Vec<ConflictSuggestion>,
}

impl std::fmt::Display for FailureExplanation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "CONFLICT: Cannot resolve {}", self.contested_package)?;
        for (pkg, range) in &self.requirements {
            writeln!(
                f,
                "  - {} requires {} {}",
                pkg, self.contested_package, range
            )?;
        }
        writeln!(f)?;
        writeln!(f, "Suggestions:")?;
        for (i, suggestion) in self.suggestions.iter().enumerate() {
            write!(f, "  {}. ", i + 1)?;
            if suggestion.source != "vyp" {
                write!(f, "[{}] ", suggestion.source)?;
            }
            write!(f, "{}", suggestion.message)?;
            if let Some(cmd) = &suggestion.command {
                write!(f, ": {}", cmd)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}
