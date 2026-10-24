use serde::{Deserialize, Serialize};
use std::fmt;

/// A named conflict declaration between mutually exclusive sides.
///
/// When `transitive` is true, this declaration propagates to consumers
/// of the declaring package -- the core innovation of Vyp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictDeclaration {
    pub name: String,
    /// The mutually exclusive sides of this conflict (e.g., `["gpu", "cpu"]`).
    pub sides: Vec<String>,
    /// The packages this conflict applies to.
    pub on: Vec<String>,
    /// Whether this conflict propagates up the dependency graph.
    pub transitive: bool,
    /// The package that originally declared this conflict (for inherited conflicts).
    pub origin: Option<String>,
    /// The chain of packages through which this conflict was inherited.
    pub propagation_path: Vec<String>,
}

impl ConflictDeclaration {
    pub fn new(name: impl Into<String>, sides: Vec<String>, on: Vec<String>) -> Self {
        Self {
            name: name.into(),
            sides,
            on,
            transitive: false,
            origin: None,
            propagation_path: Vec::new(),
        }
    }

    pub fn with_transitive(mut self, transitive: bool) -> Self {
        self.transitive = transitive;
        self
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    /// Create an inherited copy of this declaration, extending the propagation path.
    pub fn inherit_through(&self, package_name: &str) -> Self {
        let mut inherited = self.clone();
        if inherited.origin.is_none() {
            inherited.origin = Some(package_name.to_string());
        }
        inherited.propagation_path.push(package_name.to_string());
        inherited
    }
}

impl fmt::Display for ConflictDeclaration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "conflict '{}' on [{}] between sides [{}]",
            self.name,
            self.on.join(", "),
            self.sides.join(", ")
        )?;
        if self.transitive {
            write!(f, " (transitive)")?;
        }
        if let Some(origin) = &self.origin {
            write!(f, " [from {}]", origin)?;
        }
        Ok(())
    }
}

/// A side within a conflict, mapping to a set of dependency constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictSide {
    pub name: String,
    pub constraints: Vec<String>,
}

/// A set of conflict declarations collected from the dependency graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConflictSet {
    pub declarations: Vec<ConflictDeclaration>,
}

impl ConflictSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, decl: ConflictDeclaration) {
        if !self
            .declarations
            .iter()
            .any(|d| d.name == decl.name && d.origin == decl.origin)
        {
            self.declarations.push(decl);
        }
    }

    pub fn merge(&mut self, other: &ConflictSet) {
        for decl in &other.declarations {
            self.add(decl.clone());
        }
    }

    /// Get all transitive conflicts that should propagate to consumers.
    pub fn transitive_declarations(&self) -> Vec<&ConflictDeclaration> {
        self.declarations.iter().filter(|d| d.transitive).collect()
    }

    /// Get conflicts affecting a specific package.
    pub fn conflicts_for(&self, package_name: &str) -> Vec<&ConflictDeclaration> {
        self.declarations
            .iter()
            .filter(|d| d.on.iter().any(|p| p == package_name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_conflict_declaration() {
        let decl = ConflictDeclaration::new(
            "numpy-split",
            vec!["ml-legacy".into(), "data-modern".into()],
            vec!["numpy".into()],
        )
        .with_transitive(true);

        assert!(decl.transitive);
        assert_eq!(decl.on, vec!["numpy"]);
    }

    #[test]
    fn test_inheritance() {
        let original = ConflictDeclaration::new(
            "numpy-split",
            vec!["gpu".into(), "cpu".into()],
            vec!["numpy".into()],
        )
        .with_transitive(true);

        let inherited = original.inherit_through("package-a");
        assert_eq!(inherited.origin.as_deref(), Some("package-a"));
        assert_eq!(inherited.propagation_path, vec!["package-a"]);

        let double_inherited = inherited.inherit_through("package-b");
        assert_eq!(double_inherited.origin.as_deref(), Some("package-a"));
        assert_eq!(
            double_inherited.propagation_path,
            vec!["package-a", "package-b"]
        );
    }

    #[test]
    fn test_conflict_set() {
        let mut set = ConflictSet::new();
        let decl = ConflictDeclaration::new(
            "numpy-split",
            vec!["a".into(), "b".into()],
            vec!["numpy".into()],
        )
        .with_transitive(true);

        set.add(decl.clone());
        set.add(decl);
        assert_eq!(set.declarations.len(), 1);

        assert_eq!(set.transitive_declarations().len(), 1);
        assert_eq!(set.conflicts_for("numpy").len(), 1);
        assert_eq!(set.conflicts_for("pandas").len(), 0);
    }
}
