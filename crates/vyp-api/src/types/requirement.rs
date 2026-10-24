use crate::types::marker::MarkerTree;
use crate::types::package::VypPackage;
use crate::types::version::VypVersion;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// A single version comparator, e.g. `>=1.2.3` or `<2.0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionConstraint {
    pub op: ComparisonOp,
    pub version: VypVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComparisonOp {
    Eq,
    NotEq,
    Gte,
    Lte,
    Gt,
    Lt,
    /// Compatible release (~=), e.g. ~=1.4 means >=1.4, <2.0
    Compatible,
    /// Arbitrary equality (===), discouraged but part of PEP 440
    ArbitraryEq,
    /// Wildcard equality (==X.Y.*), matches any version with prefix X.Y
    EqStar,
    /// Wildcard not-equal (!=X.Y.*), excludes any version with prefix X.Y
    NotEqStar,
}

/// A PEP 508 dependency requirement: package name + optional extras +
/// version specifier or URL + optional environment markers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    pub package: VypPackage,
    pub extras: Vec<String>,
    pub constraints: Vec<VersionConstraint>,
    /// PEP 508 URL requirement (e.g., `package @ https://...`)
    pub url: Option<String>,
    pub marker: Option<MarkerTree>,
}

impl Requirement {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            package: VypPackage::named(name),
            extras: Vec::new(),
            constraints: Vec::new(),
            url: None,
            marker: None,
        }
    }

    pub fn with_constraint(mut self, op: ComparisonOp, version: VypVersion) -> Self {
        self.constraints.push(VersionConstraint { op, version });
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    pub fn with_extras(mut self, extras: Vec<String>) -> Self {
        self.extras = extras;
        self
    }

    pub fn with_marker(mut self, marker: impl Into<String>) -> Self {
        let s: String = marker.into();
        let tree = MarkerTree::parse(&s);
        self.marker = Some(tree);
        self
    }

    pub fn with_marker_tree(mut self, tree: MarkerTree) -> Self {
        self.marker = Some(tree);
        self
    }

    /// Check if a specific version satisfies all constraints.
    pub fn satisfied_by(&self, version: &VypVersion) -> bool {
        self.constraints.iter().all(|c| c.satisfied_by(version))
    }

    /// Returns true if this is a URL requirement (`pkg @ url`).
    pub fn is_url(&self) -> bool {
        self.url.is_some()
    }
}

impl VersionConstraint {
    pub fn satisfied_by(&self, version: &VypVersion) -> bool {
        match self.op {
            ComparisonOp::Eq => {
                // PEP 440: == with a local version matches only that exact
                // local; == without local matches any local of the same public.
                if self.version.is_local() {
                    version == &self.version
                } else {
                    version.without_local() == self.version.without_local()
                }
            }
            ComparisonOp::NotEq => {
                if self.version.is_local() {
                    version != &self.version
                } else {
                    version.without_local() != self.version.without_local()
                }
            }
            ComparisonOp::Gte => version >= &self.version,
            ComparisonOp::Lte => version <= &self.version,
            ComparisonOp::Gt => version > &self.version,
            ComparisonOp::Lt => version < &self.version,
            ComparisonOp::Compatible => {
                if version < &self.version {
                    return false;
                }
                // ~=X.Y.Z means >=X.Y.Z, <X.(Y+1).0
                // ~=X.Y means >=X.Y, <(X+1).0
                let rel = &self.version.release;
                if rel.len() >= 2 {
                    let mut upper = rel[..rel.len() - 1].to_vec();
                    if let Some(last) = upper.last_mut() {
                        *last += 1;
                    }
                    let upper_version = VypVersion::new(upper);
                    version < &upper_version
                } else {
                    true
                }
            }
            ComparisonOp::ArbitraryEq => version.to_string() == self.version.to_string(),
            ComparisonOp::EqStar => {
                let prefix = &self.version.release;
                if version.release.len() < prefix.len() {
                    return false;
                }
                version.release[..prefix.len()] == prefix[..]
            }
            ComparisonOp::NotEqStar => {
                let prefix = &self.version.release;
                if version.release.len() < prefix.len() {
                    return true;
                }
                version.release[..prefix.len()] != prefix[..]
            }
        }
    }
}

impl fmt::Display for ComparisonOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eq => write!(f, "=="),
            Self::NotEq => write!(f, "!="),
            Self::Gte => write!(f, ">="),
            Self::Lte => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::Lt => write!(f, "<"),
            Self::Compatible => write!(f, "~="),
            Self::ArbitraryEq => write!(f, "==="),
            Self::EqStar => write!(f, "=="),
            Self::NotEqStar => write!(f, "!="),
        }
    }
}

impl fmt::Display for VersionConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.op {
            ComparisonOp::EqStar => write!(f, "=={}.*", self.version),
            ComparisonOp::NotEqStar => write!(f, "!={}.*", self.version),
            _ => write!(f, "{}{}", self.op, self.version),
        }
    }
}

impl fmt::Display for Requirement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.package)?;
        if !self.extras.is_empty() {
            write!(f, "[{}]", self.extras.join(","))?;
        }
        if let Some(url) = &self.url {
            write!(f, " @ {}", url)?;
        } else if !self.constraints.is_empty() {
            let specs: Vec<String> = self.constraints.iter().map(|c| c.to_string()).collect();
            write!(f, "{}", specs.join(","))?;
        }
        if let Some(tree) = &self.marker {
            write!(f, " ; {}", tree)?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid requirement: {0}")]
pub struct RequirementParseError(pub String);

impl FromStr for Requirement {
    type Err = RequirementParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();

        // Split off environment markers at ';'
        let (main, marker) = split_marker(s);

        let main = main.trim();
        if main.is_empty() {
            return Err(RequirementParseError(s.to_string()));
        }

        // Parse name and optional extras
        let (name, extras, rest) =
            parse_name_extras(main).map_err(|_| RequirementParseError(s.to_string()))?;

        let rest = rest.trim();

        // Check for URL requirement: `@ <url>`
        if let Some(url_part) = rest.strip_prefix('@') {
            let url = url_part.trim().to_string();
            if url.is_empty() {
                return Err(RequirementParseError(s.to_string()));
            }
            return Ok(Requirement {
                package: VypPackage::named(name),
                extras,
                constraints: Vec::new(),
                url: Some(url),
                marker,
            });
        }

        // Parse version constraints (may start with '(' for parenthesized specs)
        let version_str = if rest.starts_with('(') && rest.ends_with(')') {
            &rest[1..rest.len() - 1]
        } else {
            rest
        };

        let constraints = parse_version_constraints(version_str)
            .map_err(|_| RequirementParseError(s.to_string()))?;

        Ok(Requirement {
            package: VypPackage::named(name),
            extras,
            constraints,
            url: None,
            marker,
        })
    }
}

/// Split off the marker expression after the first unquoted `;` and parse it.
fn split_marker(s: &str) -> (&str, Option<MarkerTree>) {
    let mut in_quote = false;
    let mut quote_char = ' ';
    for (i, c) in s.char_indices() {
        if in_quote {
            if c == quote_char {
                in_quote = false;
            }
            continue;
        }
        if c == '\'' || c == '"' {
            in_quote = true;
            quote_char = c;
            continue;
        }
        if c == ';' {
            let marker_str = s[i + 1..].trim();
            let marker = if marker_str.is_empty() {
                None
            } else {
                Some(MarkerTree::parse(marker_str))
            };
            return (&s[..i], marker);
        }
    }
    (s, None)
}

/// Extract the package name, optional extras, and the remaining string.
fn parse_name_extras(s: &str) -> Result<(&str, Vec<String>, &str), ()> {
    if let Some(bracket_start) = s.find('[') {
        let bracket_end = s.find(']').ok_or(())?;
        if bracket_end < bracket_start {
            return Err(());
        }
        let name = s[..bracket_start].trim();
        let extras_str = &s[bracket_start + 1..bracket_end];
        let extras: Vec<String> = extras_str
            .split(',')
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        let rest = s[bracket_end + 1..].trim();
        Ok((name, extras, rest))
    } else {
        // Name ends at first version specifier char, '@', or whitespace before '@'
        let name_end = s
            .find(|c: char| {
                c == '>' || c == '<' || c == '!' || c == '=' || c == '~' || c == '(' || c == '@'
            })
            .unwrap_or(s.len());
        let name = s[..name_end].trim();
        let rest = s[name_end..].trim();
        Ok((name, Vec::new(), rest))
    }
}

fn parse_version_constraints(s: &str) -> Result<Vec<VersionConstraint>, ()> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(Vec::new());
    }

    let mut constraints = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        let (op, ver_str) = if let Some(rest) = part.strip_prefix("~=") {
            (ComparisonOp::Compatible, rest.trim())
        } else if let Some(rest) = part.strip_prefix("===") {
            (ComparisonOp::ArbitraryEq, rest.trim())
        } else if let Some(rest) = part.strip_prefix(">=") {
            (ComparisonOp::Gte, rest.trim())
        } else if let Some(rest) = part.strip_prefix("<=") {
            (ComparisonOp::Lte, rest.trim())
        } else if let Some(rest) = part.strip_prefix("!=") {
            let rest = rest.trim();
            if let Some(prefix) = rest.strip_suffix(".*") {
                let version: VypVersion = prefix.trim().parse().map_err(|_| ())?;
                constraints.push(VersionConstraint {
                    op: ComparisonOp::NotEqStar,
                    version,
                });
                continue;
            }
            (ComparisonOp::NotEq, rest)
        } else if let Some(rest) = part.strip_prefix("==") {
            let rest = rest.trim();
            if let Some(prefix) = rest.strip_suffix(".*") {
                let version: VypVersion = prefix.trim().parse().map_err(|_| ())?;
                constraints.push(VersionConstraint {
                    op: ComparisonOp::EqStar,
                    version,
                });
                continue;
            }
            (ComparisonOp::Eq, rest)
        } else if let Some(rest) = part.strip_prefix('>') {
            (ComparisonOp::Gt, rest.trim())
        } else if let Some(rest) = part.strip_prefix('<') {
            (ComparisonOp::Lt, rest.trim())
        } else {
            return Err(());
        };

        let version: VypVersion = ver_str.parse().map_err(|_| ())?;
        constraints.push(VersionConstraint { op, version });
    }

    Ok(constraints)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let req: Requirement = "numpy>=1.20".parse().unwrap();
        assert_eq!(req.package, VypPackage::named("numpy"));
        assert_eq!(req.constraints.len(), 1);
        assert_eq!(req.constraints[0].op, ComparisonOp::Gte);
    }

    #[test]
    fn test_parse_with_extras() {
        let req: Requirement = "requests[security]>=2.0".parse().unwrap();
        assert_eq!(req.extras, vec!["security"]);
    }

    #[test]
    fn test_parse_multiple_extras() {
        let req: Requirement = "package[extra1, extra2]>=1.0".parse().unwrap();
        assert_eq!(req.extras, vec!["extra1", "extra2"]);
    }

    #[test]
    fn test_parse_with_marker() {
        let req: Requirement = "numpy>=1.20 ; python_version >= \"3.8\"".parse().unwrap();
        assert!(req.marker.is_some());
        // Marker is now a parsed tree, not a raw string
        let tree = req.marker.unwrap();
        assert!(matches!(tree, MarkerTree::Compare { .. }));
    }

    #[test]
    fn test_parse_range() {
        let req: Requirement = "numpy>=1.20, <2.0".parse().unwrap();
        assert_eq!(req.constraints.len(), 2);
    }

    #[test]
    fn test_satisfied_by() {
        let req: Requirement = "numpy>=1.20, <2.0".parse().unwrap();
        let v1 = VypVersion::from_parts(1, 24, 0);
        let v2 = VypVersion::from_parts(2, 0, 0);
        let v3 = VypVersion::from_parts(1, 19, 0);
        assert!(req.satisfied_by(&v1));
        assert!(!req.satisfied_by(&v2));
        assert!(!req.satisfied_by(&v3));
    }

    #[test]
    fn test_parse_url_requirement() {
        let req: Requirement = "mypackage @ https://example.com/mypackage-1.0.tar.gz"
            .parse()
            .unwrap();
        assert!(req.is_url());
        assert_eq!(req.url.unwrap(), "https://example.com/mypackage-1.0.tar.gz");
        assert!(req.constraints.is_empty());
    }

    #[test]
    fn test_parse_url_with_extras_and_marker() {
        let req: Requirement =
            "mypackage[extra1] @ https://example.com/pkg.tar.gz ; python_version >= \"3.9\""
                .parse()
                .unwrap();
        assert!(req.is_url());
        assert_eq!(req.extras, vec!["extra1"]);
        assert!(req.marker.is_some());
    }

    #[test]
    fn test_parse_compatible_release() {
        let req: Requirement = "numpy~=1.4.2".parse().unwrap();
        assert_eq!(req.constraints.len(), 1);
        assert_eq!(req.constraints[0].op, ComparisonOp::Compatible);
        // ~=1.4.2 means >=1.4.2, <1.5.0
        let v_good = VypVersion::from_parts(1, 4, 5);
        let v_bad = VypVersion::from_parts(1, 5, 0);
        assert!(req.satisfied_by(&v_good));
        assert!(!req.satisfied_by(&v_bad));
    }

    #[test]
    fn test_parse_parenthesized() {
        let req: Requirement = "numpy (>=1.20, <2.0)".parse().unwrap();
        assert_eq!(req.constraints.len(), 2);
    }

    #[test]
    fn test_display_url_requirement() {
        let req = Requirement::new("pkg")
            .with_url("https://example.com/pkg.tar.gz")
            .with_marker("python_version >= \"3.8\"");
        let s = req.to_string();
        assert!(s.contains("@ https://example.com/pkg.tar.gz"));
        assert!(s.contains("; python_version"));
    }

    #[test]
    fn test_arbitrary_equality() {
        let req: Requirement = "numpy===1.0.0".parse().unwrap();
        assert_eq!(req.constraints.len(), 1);
        assert_eq!(req.constraints[0].op, ComparisonOp::ArbitraryEq);
    }

    #[test]
    fn test_parse_no_version() {
        let req: Requirement = "numpy".parse().unwrap();
        assert_eq!(req.package, VypPackage::named("numpy"));
        assert!(req.constraints.is_empty());
    }
}
