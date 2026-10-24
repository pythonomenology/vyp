use super::lockfile::LockFile;
use std::collections::BTreeSet;
use std::fmt;

/// Differences between two PEP 751 lock files.
#[derive(Debug)]
pub struct LockFileDiff {
    pub added: Vec<PackageChange>,
    pub removed: Vec<PackageChange>,
    pub changed: Vec<VersionChange>,
    pub unchanged: usize,
}

#[derive(Debug)]
pub struct PackageChange {
    pub name: String,
    pub version: String,
}

#[derive(Debug)]
pub struct VersionChange {
    pub name: String,
    pub old_version: String,
    pub new_version: String,
}

pub fn diff_lockfiles(old: &LockFile, new: &LockFile) -> LockFileDiff {
    let old_pkgs: std::collections::BTreeMap<&str, &str> = old
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();
    let new_pkgs: std::collections::BTreeMap<&str, &str> = new
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p.version.as_str()))
        .collect();

    let old_names: BTreeSet<_> = old_pkgs.keys().copied().collect();
    let new_names: BTreeSet<_> = new_pkgs.keys().copied().collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = 0;

    for name in new_names.difference(&old_names) {
        added.push(PackageChange {
            name: name.to_string(),
            version: new_pkgs[name].to_string(),
        });
    }

    for name in old_names.difference(&new_names) {
        removed.push(PackageChange {
            name: name.to_string(),
            version: old_pkgs[name].to_string(),
        });
    }

    for name in old_names.intersection(&new_names) {
        let old_ver = old_pkgs[name];
        let new_ver = new_pkgs[name];
        if old_ver != new_ver {
            changed.push(VersionChange {
                name: name.to_string(),
                old_version: old_ver.to_string(),
                new_version: new_ver.to_string(),
            });
        } else {
            unchanged += 1;
        }
    }

    LockFileDiff {
        added,
        removed,
        changed,
        unchanged,
    }
}

impl fmt::Display for LockFileDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty() {
            writeln!(f, "No changes ({} packages unchanged)", self.unchanged)?;
            return Ok(());
        }

        if !self.added.is_empty() {
            writeln!(f, "Added:")?;
            for pkg in &self.added {
                writeln!(f, "  + {} {}", pkg.name, pkg.version)?;
            }
        }

        if !self.removed.is_empty() {
            writeln!(f, "Removed:")?;
            for pkg in &self.removed {
                writeln!(f, "  - {} {}", pkg.name, pkg.version)?;
            }
        }

        if !self.changed.is_empty() {
            writeln!(f, "Changed:")?;
            for ch in &self.changed {
                writeln!(
                    f,
                    "  ~ {} {} -> {}",
                    ch.name, ch.old_version, ch.new_version
                )?;
            }
        }

        writeln!(f, "\n{} unchanged", self.unchanged)?;
        Ok(())
    }
}
