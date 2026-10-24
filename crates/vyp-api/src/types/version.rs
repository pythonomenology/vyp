use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A PEP 440-compatible version number.
///
/// Stores epoch, release segments, pre-release, post-release, dev,
/// and local version segments per the full PEP 440 specification.
#[derive(Debug, Clone, Eq, Serialize, Deserialize)]
pub struct VypVersion {
    pub epoch: u32,
    pub release: Vec<u32>,
    pub pre: Option<PreRelease>,
    pub post: Option<u32>,
    pub dev: Option<u32>,
    /// Local version label segments (e.g., `+local.1`). Stored as normalized
    /// lowercase strings. Per PEP 440, local versions are compared
    /// lexicographically/numerically segment-by-segment but are ignored
    /// for ordering in specifier matching (only equality checks).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum PreReleaseKind {
    Alpha,
    Beta,
    Rc,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PreRelease {
    pub kind: PreReleaseKind,
    pub number: u32,
}

impl VypVersion {
    pub fn new(release: Vec<u32>) -> Self {
        Self {
            epoch: 0,
            release,
            pre: None,
            post: None,
            dev: None,
            local: Vec::new(),
        }
    }

    pub fn from_parts(major: u32, minor: u32, patch: u32) -> Self {
        Self::new(vec![major, minor, patch])
    }

    pub fn is_pre_release(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    pub fn is_local(&self) -> bool {
        !self.local.is_empty()
    }

    /// Returns the "next" version by bumping the last release segment.
    pub fn bump(&self) -> Self {
        let mut release = self.release.clone();
        if let Some(last) = release.last_mut() {
            *last += 1;
        }
        Self {
            epoch: self.epoch,
            release,
            pre: None,
            post: None,
            dev: None,
            local: Vec::new(),
        }
    }

    /// The lowest possible version: 0
    pub fn lowest() -> Self {
        Self::new(vec![0])
    }

    /// Return a copy with local segments stripped (the "public" version).
    pub fn without_local(&self) -> Self {
        Self {
            epoch: self.epoch,
            release: self.release.clone(),
            pre: self.pre.clone(),
            post: self.post,
            dev: self.dev,
            local: Vec::new(),
        }
    }

    /// Return a copy with dev release set to the given value.
    pub fn with_dev(mut self, n: u32) -> Self {
        self.dev = Some(n);
        self
    }

    /// Return a copy with post set to u32::MAX as a sentinel upper bound.
    pub fn with_post_max(mut self) -> Self {
        self.post = Some(u32::MAX);
        self
    }

    /// Return a copy with local set to a max sentinel.
    pub fn with_local_max(mut self) -> Self {
        self.local = vec!["zzzzzzzz".to_string()];
        self
    }

    fn release_tuple(&self, len: usize) -> Vec<u32> {
        let mut r = self.release.clone();
        r.resize(len, 0);
        r
    }
}

/// PEP 440 specifies that local versions are equal to non-local for
/// ordering purposes, but we need Hash+Eq to consider local segments
/// so that HashMap can distinguish `1.0+local1` from `1.0+local2`.
impl PartialEq for VypVersion {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && {
                let max_len = self.release.len().max(other.release.len());
                self.release_tuple(max_len) == other.release_tuple(max_len)
            }
            && self.pre == other.pre
            && self.post == other.post
            && self.dev == other.dev
            && self.local == other.local
    }
}

impl std::hash::Hash for VypVersion {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.epoch.hash(state);
        let max_len = self.release.len().max(1);
        self.release_tuple(max_len).hash(state);
        self.pre.hash(state);
        self.post.hash(state);
        self.dev.hash(state);
        self.local.hash(state);
    }
}

impl Ord for VypVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| {
                let max_len = self.release.len().max(other.release.len());
                let a = self.release_tuple(max_len);
                let b = other.release_tuple(max_len);
                a.cmp(&b)
            })
            .then_with(|| cmp_pre(&self.pre, &other.pre))
            .then_with(|| self.post.cmp(&other.post))
            .then_with(|| cmp_dev(&self.dev, &other.dev))
            .then_with(|| cmp_local(&self.local, &other.local))
    }
}

impl PartialOrd for VypVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Pre-release ordering: None (final release) > Some(rc) > Some(beta) > Some(alpha)
fn cmp_pre(a: &Option<PreRelease>, b: &Option<PreRelease>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => {
            let kind_ord = |k: &PreReleaseKind| match k {
                PreReleaseKind::Alpha => 0,
                PreReleaseKind::Beta => 1,
                PreReleaseKind::Rc => 2,
            };
            kind_ord(&a.kind)
                .cmp(&kind_ord(&b.kind))
                .then_with(|| a.number.cmp(&b.number))
        }
    }
}

/// Dev ordering: None (final) > Some(N), lower N is earlier dev release
fn cmp_dev(a: &Option<u32>, b: &Option<u32>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    }
}

/// Local version ordering per PEP 440: no local < has local, then
/// compare segments numerically when both parse as integers, otherwise
/// lexicographic. Fewer segments < more segments when prefixes match.
fn cmp_local(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => {
            for (sa, sb) in a.iter().zip(b.iter()) {
                let ord = match (sa.parse::<u64>(), sb.parse::<u64>()) {
                    (Ok(na), Ok(nb)) => na.cmp(&nb),
                    _ => sa.cmp(sb),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            a.len().cmp(&b.len())
        }
    }
}

impl fmt::Display for VypVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.epoch != 0 {
            write!(f, "{}!", self.epoch)?;
        }
        let release: Vec<String> = self.release.iter().map(|s| s.to_string()).collect();
        write!(f, "{}", release.join("."))?;
        if let Some(pre) = &self.pre {
            let tag = match pre.kind {
                PreReleaseKind::Alpha => "a",
                PreReleaseKind::Beta => "b",
                PreReleaseKind::Rc => "rc",
            };
            write!(f, "{}{}", tag, pre.number)?;
        }
        if let Some(post) = self.post {
            write!(f, ".post{}", post)?;
        }
        if let Some(dev) = self.dev {
            write!(f, ".dev{}", dev)?;
        }
        if !self.local.is_empty() {
            write!(f, "+{}", self.local.join("."))?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid version string: {0}")]
pub struct VersionParseError(String);

impl FromStr for VypVersion {
    type Err = VersionParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // PEP 440 normalization: strip leading 'v' or 'V'
        let s = s
            .strip_prefix('v')
            .or_else(|| s.strip_prefix('V'))
            .unwrap_or(s);

        let (epoch, rest) = if let Some(idx) = s.find('!') {
            let e = s[..idx]
                .parse::<u32>()
                .map_err(|_| VersionParseError(s.to_string()))?;
            (e, &s[idx + 1..])
        } else {
            (0, s)
        };

        // Split off local version at '+'
        let (version_part, local) = if let Some(idx) = rest.find('+') {
            let local_str = &rest[idx + 1..];
            let local_segments: Vec<String> =
                local_str.split('.').map(|seg| seg.to_lowercase()).collect();
            (&rest[..idx], local_segments)
        } else {
            (rest, Vec::new())
        };

        let mut release_str = version_part;
        let mut pre = None;
        let mut post = None;
        let mut dev = None;

        // Parse .devN (also handle "devN" without dot for normalization)
        if let Some(idx) = release_str.to_lowercase().find(".dev") {
            let dev_num = release_str[idx + 4..].parse::<u32>().unwrap_or(0);
            dev = Some(dev_num);
            release_str = &release_str[..idx];
        } else if let Some(idx) = release_str.to_lowercase().find("dev") {
            if (idx > 0 && release_str.as_bytes()[idx - 1] == b'.') || idx == 0 {
                let dev_num = release_str[idx + 3..].parse::<u32>().unwrap_or(0);
                dev = Some(dev_num);
                release_str = release_str[..idx].trim_end_matches('.');
            }
        }

        // Parse .postN (also handle "postN", "-N" post-release notation)
        if let Some(idx) = release_str.to_lowercase().find(".post") {
            let end = release_str.len();
            let post_num = release_str[idx + 5..end].parse::<u32>().unwrap_or(0);
            post = Some(post_num);
            release_str = &release_str[..idx];
        } else if let Some(idx) = release_str.to_lowercase().find("post") {
            if idx > 0 {
                let post_num = release_str[idx + 4..].parse::<u32>().unwrap_or(0);
                post = Some(post_num);
                release_str = release_str[..idx].trim_end_matches('.');
            }
        }

        // Parse pre-release: aN/alphaN, bN/betaN, rcN/cN/previewN
        let lower = release_str.to_lowercase();
        for (tags, kind) in [
            (&["rc", "c", "preview"][..], PreReleaseKind::Rc),
            (&["beta", "b"][..], PreReleaseKind::Beta),
            (&["alpha", "a"][..], PreReleaseKind::Alpha),
        ] {
            let mut found = false;
            for tag in tags {
                // Search the lowercase version for the tag after a digit
                if let Some(idx) = lower.rfind(tag) {
                    if idx > 0 && lower.as_bytes()[idx - 1].is_ascii_digit() {
                        let after = &release_str[idx + tag.len()..];
                        let num = if after.is_empty() {
                            0
                        } else if let Ok(n) = after.parse::<u32>() {
                            n
                        } else {
                            continue;
                        };
                        // Handle dot-separated pre-release (e.g., ".rc1")
                        let pre_start = if idx > 1 && release_str.as_bytes()[idx - 1] == b'.' {
                            idx - 1
                        } else {
                            idx
                        };
                        pre = Some(PreRelease { kind, number: num });
                        release_str = &release_str[..pre_start];
                        found = true;
                        break;
                    }
                }
            }
            if found {
                break;
            }
        }

        // Normalize separators: replace '_' and '-' in release with '.'
        let normalized_release: String = release_str
            .chars()
            .map(|c| if c == '_' || c == '-' { '.' } else { c })
            .collect();

        let release: Result<Vec<u32>, _> = normalized_release
            .split('.')
            .filter(|s| !s.is_empty())
            .map(|seg| {
                // Strip leading zeros per PEP 440 normalization
                let seg = seg.trim_start_matches('0');
                let seg = if seg.is_empty() { "0" } else { seg };
                seg.parse::<u32>()
                    .map_err(|_| VersionParseError(s.to_string()))
            })
            .collect();

        let release = release?;
        if release.is_empty() {
            return Err(VersionParseError(s.to_string()));
        }

        Ok(VypVersion {
            epoch,
            release,
            pre,
            post,
            dev,
            local,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let v: VypVersion = "1.2.3".parse().unwrap();
        assert_eq!(v.release, vec![1, 2, 3]);
        assert_eq!(v.epoch, 0);
    }

    #[test]
    fn test_parse_epoch() {
        let v: VypVersion = "2!1.0".parse().unwrap();
        assert_eq!(v.epoch, 2);
        assert_eq!(v.release, vec![1, 0]);
    }

    #[test]
    fn test_parse_pre() {
        let v: VypVersion = "1.0rc1".parse().unwrap();
        assert!(v.pre.is_some());
        assert_eq!(v.pre.unwrap().number, 1);
    }

    #[test]
    fn test_ordering() {
        let v1: VypVersion = "1.0.0".parse().unwrap();
        let v2: VypVersion = "1.0.1".parse().unwrap();
        let v3: VypVersion = "2.0.0".parse().unwrap();
        assert!(v1 < v2);
        assert!(v2 < v3);
    }

    #[test]
    fn test_pre_release_ordering() {
        let alpha: VypVersion = "1.0a1".parse().unwrap();
        let beta: VypVersion = "1.0b1".parse().unwrap();
        let rc: VypVersion = "1.0rc1".parse().unwrap();
        let final_v: VypVersion = "1.0.0".parse().unwrap();
        assert!(alpha < beta);
        assert!(beta < rc);
        assert!(rc < final_v);
    }

    #[test]
    fn test_display_roundtrip() {
        let v: VypVersion = "1.2.3".parse().unwrap();
        assert_eq!(v.to_string(), "1.2.3");
    }

    #[test]
    fn test_bump() {
        let v = VypVersion::from_parts(1, 2, 3);
        let bumped = v.bump();
        assert_eq!(bumped.to_string(), "1.2.4");
    }

    #[test]
    fn test_parse_local_version() {
        let v: VypVersion = "1.0+local.1".parse().unwrap();
        assert_eq!(v.release, vec![1, 0]);
        assert_eq!(v.local, vec!["local", "1"]);
        assert!(v.is_local());
    }

    #[test]
    fn test_local_version_display() {
        let v: VypVersion = "1.0+ubuntu.1".parse().unwrap();
        assert_eq!(v.to_string(), "1.0+ubuntu.1");
    }

    #[test]
    fn test_local_version_ordering() {
        let v1: VypVersion = "1.0".parse().unwrap();
        let v2: VypVersion = "1.0+local".parse().unwrap();
        // A version with local > version without local (same public version)
        assert!(v2 > v1);
    }

    #[test]
    fn test_local_version_numeric_compare() {
        let v1: VypVersion = "1.0+1".parse().unwrap();
        let v2: VypVersion = "1.0+2".parse().unwrap();
        assert!(v1 < v2);
    }

    #[test]
    fn test_without_local() {
        let v: VypVersion = "1.0+local.1".parse().unwrap();
        let public = v.without_local();
        assert!(!public.is_local());
        assert_eq!(public.to_string(), "1.0");
    }

    #[test]
    fn test_post_release() {
        let v: VypVersion = "1.0.post1".parse().unwrap();
        assert_eq!(v.post, Some(1));
        assert_eq!(v.to_string(), "1.0.post1");
    }

    #[test]
    fn test_dev_release() {
        let v: VypVersion = "1.0.dev3".parse().unwrap();
        assert_eq!(v.dev, Some(3));
        assert_eq!(v.to_string(), "1.0.dev3");
    }

    #[test]
    fn test_full_version() {
        let v: VypVersion = "1!2.3a4.post5.dev6+local.7".parse().unwrap();
        assert_eq!(v.epoch, 1);
        assert_eq!(v.release, vec![2, 3]);
        assert!(v.pre.is_some());
        assert_eq!(v.post, Some(5));
        assert_eq!(v.dev, Some(6));
        assert_eq!(v.local, vec!["local", "7"]);
    }
}
