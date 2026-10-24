use serde::{Deserialize, Serialize};
use std::fmt;

/// Identifies a package in the dependency graph.
///
/// This can represent a real package from a registry, a virtual capability
/// (for package substitution), or the root project itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VypPackage {
    /// The root project being resolved.
    Root,
    /// A real package identified by its normalized name.
    Named(String),
    /// A virtual capability provided by one of several substitute packages.
    Virtual(String),
}

impl VypPackage {
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named(normalize_package_name(&name.into()))
    }

    pub fn virt(capability: impl Into<String>) -> Self {
        Self::Virtual(capability.into())
    }

    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Root => "<root>",
            Self::Named(n) => n,
            Self::Virtual(c) => c,
        }
    }
}

impl fmt::Display for VypPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => write!(f, "<root>"),
            Self::Named(name) => write!(f, "{}", name),
            Self::Virtual(cap) => write!(f, "<virtual:{}>", cap),
        }
    }
}

/// Normalize a Python package name per PEP 503.
pub fn normalize_package_name(name: &str) -> String {
    name.to_lowercase().replace(['-', '.', ' '], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize() {
        assert_eq!(normalize_package_name("My-Package.Name"), "my_package_name");
        assert_eq!(normalize_package_name("numpy"), "numpy");
    }

    #[test]
    fn test_package_display() {
        assert_eq!(VypPackage::Root.to_string(), "<root>");
        assert_eq!(VypPackage::named("NumPy").to_string(), "numpy");
        assert_eq!(VypPackage::virt("opencv").to_string(), "<virtual:opencv>");
    }
}
