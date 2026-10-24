use std::collections::HashSet;

use vyp_api::traits::resolution_filter::{Candidate, ResolutionFilter};

use crate::PreReleasePolicy;

/// Filters pre-release versions based on the configured policy.
///
/// - `Disallow`: Always exclude pre-releases, even if no stable version exists.
/// - `IfNecessary`: Exclude pre-releases when a stable version exists, but keep
///   them as a fallback when no stable candidate is available.
/// - `Allow`: No filtering (this filter should not be registered).
#[derive(Debug)]
pub struct PreReleaseFilter {
    policy: PreReleasePolicy,
    /// Normalized names whose pre-releases are always allowed.
    allowed: HashSet<String>,
}

impl PreReleaseFilter {
    pub fn new(allow: bool) -> Self {
        Self {
            policy: if allow {
                PreReleasePolicy::Allow
            } else {
                PreReleasePolicy::Disallow
            },
            allowed: HashSet::new(),
        }
    }

    pub fn with_policy(policy: PreReleasePolicy) -> Self {
        Self {
            policy,
            allowed: HashSet::new(),
        }
    }

    /// Allow pre-releases of these packages whatever the policy.
    pub fn allowing(mut self, packages: HashSet<String>) -> Self {
        self.allowed = packages;
        self
    }
}

impl ResolutionFilter for PreReleaseFilter {
    fn name(&self) -> &str {
        "pre-release-filter"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn filter(&self, candidates: &mut Vec<Candidate>) {
        if self.policy == PreReleasePolicy::Allow || candidates.is_empty() {
            return;
        }
        if self.allowed.contains(candidates[0].package.name()) {
            return;
        }

        // IfNecessary: keep pre-releases if there are no stable candidates
        if self.policy == PreReleasePolicy::IfNecessary {
            let has_stable = candidates
                .iter()
                .any(|c| !c.excluded && !c.version.is_pre_release());
            if !has_stable {
                return;
            }
        }

        for candidate in candidates.iter_mut() {
            if candidate.version.is_pre_release() && !candidate.excluded {
                candidate.excluded = true;
                candidate.exclusion_reason = Some("pre-release excluded by policy".to_string());
            }
        }
    }
}
