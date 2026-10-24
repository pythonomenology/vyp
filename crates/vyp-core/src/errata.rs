//! Metadata errata: corrections to releases that are already published.
//!
//! A release's `Requires-Dist` and `Requires-Python` are frozen once it is on
//! an index, so a maintainer who later learns that `green<4` breaks on Python
//! 3.12, or that `foo==1.2.0` needs `bar<2`, has no way to say so. An erratum
//! records that knowledge out of band and the resolver applies it as if the
//! release had shipped with the corrected metadata.

use vyp_api::traits::resolution_filter::{Candidate, ResolutionFilter};
use vyp_api::{normalize_package_name, Requirement, VypVersion};

use crate::resolver::VS;

/// A correction applied to a range of releases of one package.
///
/// Environment-dependent fields (`Requires-Python`, requirement markers) are
/// evaluated by the caller for the target environment before the erratum
/// reaches the resolver, so an erratum here is already environment-specific.
#[derive(Debug, Clone)]
pub struct Erratum {
    /// Normalized package name.
    pub package: String,
    /// Releases this erratum applies to.
    pub versions: VS,
    /// The specifier `versions` was parsed from, for display (empty = all).
    pub versions_spec: String,
    /// Never select these releases.
    pub exclude: bool,
    /// Requirements added to these releases' dependencies.
    pub requires: Vec<Requirement>,
    pub reason: Option<String>,
    /// Feed URL or path the erratum came from.
    pub source: String,
}

impl Erratum {
    pub fn new(package: &str, versions: VS, source: impl Into<String>) -> Self {
        Self {
            package: normalize_package_name(package),
            versions,
            versions_spec: String::new(),
            exclude: false,
            requires: Vec::new(),
            reason: None,
            source: source.into(),
        }
    }

    pub fn applies_to(&self, package: &str, version: &VypVersion) -> bool {
        self.package == package && self.versions.contains(version)
    }
}

/// An errata feed and the SHA-256 of the content the resolver used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrataFeedDigest {
    pub source: String,
    pub sha256: String,
}

/// Removes releases that an erratum marks as excluded.
#[derive(Debug)]
pub struct ErrataFilter {
    errata: Vec<Erratum>,
}

impl ErrataFilter {
    pub fn new(errata: Vec<Erratum>) -> Self {
        Self {
            errata: errata.into_iter().filter(|e| e.exclude).collect(),
        }
    }
}

impl ResolutionFilter for ErrataFilter {
    fn name(&self) -> &str {
        "errata"
    }

    fn priority(&self) -> i32 {
        100
    }

    fn filter(&self, candidates: &mut Vec<Candidate>) {
        for candidate in candidates.iter_mut() {
            if candidate.excluded {
                continue;
            }
            let name = candidate.package.name();
            if let Some(erratum) = self
                .errata
                .iter()
                .find(|e| e.applies_to(name, &candidate.version))
            {
                candidate.excluded = true;
                candidate.exclusion_reason = Some(format!(
                    "excluded by erratum from {}{}",
                    erratum.source,
                    erratum
                        .reason
                        .as_deref()
                        .map(|r| format!(": {}", r))
                        .unwrap_or_default()
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use version_ranges::Ranges;
    use vyp_api::VypPackage;

    #[test]
    fn filter_excludes_only_matching_releases() {
        let mut erratum = Erratum::new(
            "Green",
            Ranges::strictly_lower_than(VypVersion::from_parts(4, 0, 0)),
            "feed.toml",
        );
        erratum.exclude = true;
        let filter = ErrataFilter::new(vec![erratum]);

        let mut candidates = vec![
            Candidate::new(VypPackage::named("green"), VypVersion::from_parts(4, 0, 1)),
            Candidate::new(VypPackage::named("green"), VypVersion::from_parts(3, 9, 0)),
            Candidate::new(VypPackage::named("other"), VypVersion::from_parts(1, 0, 0)),
        ];
        filter.filter(&mut candidates);

        let excluded: Vec<bool> = candidates.iter().map(|c| c.excluded).collect();
        assert_eq!(excluded, vec![false, true, false]);
        assert!(candidates[1]
            .exclusion_reason
            .as_deref()
            .unwrap()
            .contains("feed.toml"));
    }
}
