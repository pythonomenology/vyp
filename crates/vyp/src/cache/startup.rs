//! Gate on files that make Python run code at every interpreter start.
//!
//! `site.py` executes any line of a site-packages `*.pth` file that starts
//! with `import`, and imports top-level `sitecustomize` / `usercustomize`
//! modules on startup. A compromised release can use either to run code in
//! every process that uses the environment, not only in programs that import
//! the package (see discuss.python.org/t/106678). Path-only `.pth` lines just
//! extend `sys.path` and are not gated.
//!
//! The installer inspects each wheel before linking it and, per the
//! `startup-files` policy, warns (default), refuses, or allows such files.
//! Packages in `allow-startup-files`, a small built-in list, and the
//! project's own editable installs are always allowed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Packages whose startup files are well known and expected.
const BUILTIN_ALLOWED: &[&str] = &["setuptools", "pywin32", "distutils_precedence"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartupMode {
    #[default]
    Warn,
    Deny,
    Allow,
}

#[derive(Debug, Clone, Default)]
pub struct StartupPolicy {
    pub mode: StartupMode,
    /// Normalized names allowed to ship startup files.
    pub allowed: HashSet<String>,
}

impl StartupPolicy {
    pub fn allows(&self, package: &str) -> bool {
        let name = vyp_api::normalize_package_name(package);
        self.mode == StartupMode::Allow
            || self.allowed.contains(&name)
            || BUILTIN_ALLOWED.contains(&name.as_str())
    }
}

/// A file in a wheel that runs code at interpreter startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupFile {
    /// Path inside the wheel.
    pub path: String,
    /// For `.pth` files, the lines that execute.
    pub code: Vec<String>,
}

impl StartupFile {
    pub fn describe(&self) -> String {
        match self.code.first() {
            Some(line) => {
                let line: String = line.chars().take(80).collect();
                format!("{} (runs: {}{})", self.path, line, if self.code.len() > 1 { " ..." } else { "" })
            }
            None => format!("{} (imported at startup)", self.path),
        }
    }
}

/// The path of `rel` relative to the site-packages root, if it installs there.
fn site_packages_rel(rel: &str) -> Option<&str> {
    let mut parts = rel.splitn(3, '/');
    let first = parts.next()?;
    if first.ends_with(".data") {
        let scheme = parts.next()?;
        if scheme == "purelib" || scheme == "platlib" {
            return parts.next();
        }
        return None;
    }
    Some(rel)
}

/// Startup files in an extracted wheel.
pub fn inspect(archive_dir: &Path, files: &[PathBuf]) -> Vec<StartupFile> {
    let mut found = Vec::new();
    for file in files {
        let rel = file.to_string_lossy().replace('\\', "/");
        let Some(site_rel) = site_packages_rel(&rel) else { continue };
        if site_rel.contains('/') {
            if matches!(site_rel, "sitecustomize/__init__.py" | "usercustomize/__init__.py") {
                found.push(StartupFile { path: rel.clone(), code: Vec::new() });
            }
            continue;
        }
        if site_rel.ends_with(".pth") {
            let content = std::fs::read_to_string(archive_dir.join(file)).unwrap_or_default();
            let code: Vec<String> = content
                .lines()
                .filter(|l| l.starts_with("import ") || l.starts_with("import\t"))
                .map(|l| l.trim().to_string())
                .collect();
            if !code.is_empty() {
                found.push(StartupFile { path: rel, code });
            }
        } else if matches!(
            site_rel,
            "sitecustomize.py" | "usercustomize.py" | "sitecustomize.pyc" | "usercustomize.pyc"
        ) || site_rel.starts_with("sitecustomize.") && site_rel.ends_with(".so")
        {
            found.push(StartupFile { path: rel, code: Vec::new() });
        }
    }
    found
}

/// Apply the policy to one wheel. Returns the warning to print, if any; an
/// error when the policy refuses the package.
pub fn check(
    package: &str,
    archive_dir: &Path,
    files: &[PathBuf],
    policy: &StartupPolicy,
) -> Result<Option<String>, String> {
    if policy.allows(package) {
        return Ok(None);
    }
    let found = inspect(archive_dir, files);
    if found.is_empty() {
        return Ok(None);
    }
    let listing: Vec<String> = found.iter().map(StartupFile::describe).collect();
    let message = format!(
        "{} installs code that runs at every Python startup:\n    {}",
        package,
        listing.join("\n    ")
    );
    match policy.mode {
        StartupMode::Deny => Err(format!(
            "{}\n  Refusing to install it (startup-files = \"deny\"). Add \"{}\" to allow-startup-files if this is expected.",
            message, package
        )),
        _ => Ok(Some(format!(
            "{}\n  Add \"{}\" to allow-startup-files to silence this, or set startup-files = \"deny\" to refuse such packages.",
            message, package
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel(files: &[(&str, &str)]) -> (PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!(
            "vyp-startup-{}-{}",
            std::process::id(),
            files.len() * 7 + files.first().map(|f| f.0.len()).unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut list = Vec::new();
        for (path, content) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            list.push(PathBuf::from(path));
        }
        (dir, list)
    }

    #[test]
    fn finds_executable_pth_and_sitecustomize_only() {
        let (dir, files) = wheel(&[
            ("evil.pth", "import os; os.system('curl x')\n"),
            ("paths.pth", "/some/path\n# import commented\n"),
            ("pkg/inner.pth", "import os\n"),
            ("demo-1.0.data/purelib/sitecustomize.py", "print('hi')\n"),
            ("demo-1.0.data/scripts/x.pth", "import os\n"),
        ]);
        let found = inspect(&dir, &files);
        let paths: Vec<&str> = found.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, vec!["evil.pth", "demo-1.0.data/purelib/sitecustomize.py"]);
        assert_eq!(found[0].code, vec!["import os; os.system('curl x')"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn policy_modes() {
        let (dir, files) = wheel(&[("x.pth", "import os\n")]);
        let warn = StartupPolicy::default();
        assert!(check("demo", &dir, &files, &warn).unwrap().is_some());
        assert!(check("setuptools", &dir, &files, &warn).unwrap().is_none());

        let deny = StartupPolicy { mode: StartupMode::Deny, allowed: ["demo".to_string()].into() };
        assert!(check("demo", &dir, &files, &deny).unwrap().is_none());
        assert!(check("other", &dir, &files, &deny).is_err());

        let allow = StartupPolicy { mode: StartupMode::Allow, ..Default::default() };
        assert!(check("other", &dir, &files, &allow).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
