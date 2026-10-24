use vyp_api::traits::conflict_strategy::{
    ConflictContext, ConflictStrategy, ConflictSuggestion, RangeRewrite, StrategyVerdict,
};
use vyp_api::types::provenance::SelectionReason;
use vyp_api::types::requirement::ComparisonOp;
use vyp_api::DependencyOverride;
use vyp_api::VypVersion;

/// Built-in strategy that applies range-based dependency overrides.
#[derive(Debug)]
pub struct OverrideConflictStrategy {
    overrides: Vec<DependencyOverride>,
}

impl OverrideConflictStrategy {
    pub fn new(overrides: Vec<DependencyOverride>) -> Self {
        Self { overrides }
    }

    fn find_override(&self, package: &str) -> Option<&DependencyOverride> {
        self.overrides.iter().find(|o| o.package == package)
    }
}

impl ConflictStrategy for OverrideConflictStrategy {
    fn name(&self) -> &str {
        "override"
    }

    fn priority(&self) -> i32 {
        90
    }

    fn evaluate(&self, context: &ConflictContext) -> StrategyVerdict {
        let pkg_name = context.contested_package.name();

        if let Some(dep_override) = self.find_override(pkg_name) {
            let constraints = parse_constraint_string(&dep_override.constraint);

            let mut lower = None;
            let mut upper = None;
            let mut upper_inclusive = false;

            for constraint in &constraints {
                match constraint.op {
                    ComparisonOp::Gte => lower = Some(constraint.version.clone()),
                    ComparisonOp::Gt => lower = Some(constraint.version.bump()),
                    ComparisonOp::Lt => upper = Some(constraint.version.clone()),
                    ComparisonOp::Lte => {
                        upper = Some(constraint.version.clone());
                        upper_inclusive = true;
                    }
                    ComparisonOp::Eq => {
                        lower = Some(constraint.version.clone());
                        upper = Some(constraint.version.clone());
                        upper_inclusive = true;
                    }
                    _ => {}
                }
            }

            let rewrites = vec![RangeRewrite {
                package: pkg_name.to_string(),
                new_lower: lower,
                new_upper: upper,
                upper_inclusive,
            }];

            return StrategyVerdict::RewriteRanges { rewrites };
        }

        StrategyVerdict::Abstain
    }

    fn suggest(&self, context: &ConflictContext) -> Vec<ConflictSuggestion> {
        let pkg_name = context.contested_package.name();
        if self.find_override(pkg_name).is_some() {
            return Vec::new();
        }

        vec![ConflictSuggestion {
            source: self.name().to_string(),
            message: format!("Add an override for '{}'", pkg_name),
            command: Some(format!("vyp override add {} \"<version_spec>\"", pkg_name)),
        }]
    }

    fn selection_reason(&self) -> SelectionReason {
        SelectionReason::Override
    }
}

struct ParsedConstraint {
    op: ComparisonOp,
    version: VypVersion,
}

fn parse_constraint_string(s: &str) -> Vec<ParsedConstraint> {
    let mut constraints = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let (op, ver_str) = if let Some(s) = part.strip_prefix(">=") {
            (ComparisonOp::Gte, s)
        } else if let Some(s) = part.strip_prefix("<=") {
            (ComparisonOp::Lte, s)
        } else if let Some(s) = part.strip_prefix("!=") {
            (ComparisonOp::NotEq, s)
        } else if let Some(s) = part.strip_prefix("==") {
            (ComparisonOp::Eq, s)
        } else if let Some(s) = part.strip_prefix("~=") {
            (ComparisonOp::Compatible, s)
        } else if let Some(s) = part.strip_prefix('>') {
            (ComparisonOp::Gt, s)
        } else if let Some(s) = part.strip_prefix('<') {
            (ComparisonOp::Lt, s)
        } else {
            continue;
        };

        if let Ok(version) = ver_str.trim().parse() {
            constraints.push(ParsedConstraint { op, version });
        }
    }
    constraints
}
