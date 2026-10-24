//! Finding Python interpreters, and installing managed ones.
//!
//! Discovery looks, in order, at interpreters vyp installed itself, then at
//! every `python3.X` / `python3` / `python` on `PATH`, then at pyenv's
//! versions directory. Each candidate is probed once (the result is cached
//! by path and modification time) so vyp knows its real executable, version
//! and install scheme without trusting file names: pyenv shims, Homebrew
//! symlinks and framework builds all resolve to the binary that actually runs.

pub mod download;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use vyp_api::{Requirement, VypVersion};
use vyp_core::resolver::requirements_to_range;

use crate::cache::wheel_cache::cache_base_dir;

/// A probed Python interpreter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Interpreter {
    /// The binary that runs (symlinks resolved). For a venv interpreter this
    /// is the venv's own `python`.
    pub executable: PathBuf,
    /// The base (non-venv) interpreter binary.
    pub base_executable: PathBuf,
    /// `X.Y.Z`.
    pub version: String,
    /// `sys.implementation.name`, e.g. `cpython`.
    pub implementation: String,
    pub is_venv: bool,
    /// Built with `Py_GIL_DISABLED`.
    pub free_threaded: bool,
    pub sys_platform: String,
    pub machine: String,
    /// Install scheme paths relative to an environment root.
    pub purelib: String,
    pub scripts: String,
    pub include: String,
    /// Installed by vyp under the managed Python directory.
    #[serde(default)]
    pub managed: bool,
}

impl Interpreter {
    /// `X.Y`, with a `t` suffix for free-threaded builds.
    pub fn short_version(&self) -> String {
        let mut parts = self.version.split('.');
        let major = parts.next().unwrap_or("3");
        let minor = parts.next().unwrap_or("0");
        format!(
            "{}.{}{}",
            major,
            minor,
            if self.free_threaded { "t" } else { "" }
        )
    }

    pub fn describe(&self) -> String {
        format!(
            "{} {}{} at {}",
            self.implementation,
            self.version,
            if self.free_threaded {
                " (free-threaded)"
            } else {
                ""
            },
            self.executable.display()
        )
    }
}

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PythonRequest {
    /// Any interpreter (still subject to `requires-python`).
    Any,
    /// `3`, `3.12`, `3.12.7`, optionally with a `t` suffix for free-threaded.
    Version {
        version: String,
        free_threaded: bool,
    },
    /// A PEP 440 specifier such as `>=3.10,<3.13`.
    Specifier(String),
    /// A path to an interpreter, or a command name on `PATH`.
    Path(PathBuf),
}

impl PythonRequest {
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() || s == "any" {
            return Self::Any;
        }
        let bare = s
            .strip_prefix("cpython")
            .or_else(|| s.strip_prefix("python"))
            .unwrap_or(s)
            .trim_start_matches(['-', '@']);
        let (digits, free_threaded) = match bare.strip_suffix('t') {
            Some(d) => (d, true),
            None => (bare, false),
        };
        if !digits.is_empty()
            && digits
                .split('.')
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        {
            return Self::Version {
                version: digits.to_string(),
                free_threaded,
            };
        }
        if s.starts_with(['<', '>', '=', '!', '~']) {
            return Self::Specifier(s.to_string());
        }
        Self::Path(PathBuf::from(s))
    }

    /// Does `interp` satisfy this request?
    pub fn matches(&self, interp: &Interpreter) -> bool {
        match self {
            Self::Any | Self::Path(_) => true,
            Self::Version {
                version,
                free_threaded,
            } => {
                if *free_threaded != interp.free_threaded {
                    return false;
                }
                let want: Vec<&str> = version.split('.').collect();
                let have: Vec<&str> = interp.version.split('.').collect();
                want.iter().zip(have.iter()).all(|(w, h)| w == h) && have.len() >= want.len()
            }
            Self::Specifier(spec) => version_satisfies(&interp.version, spec),
        }
    }
}

/// Whether `version` satisfies a `Requires-Python`-style specifier.
pub fn version_satisfies(version: &str, spec: &str) -> bool {
    let Ok(v) = version.parse::<VypVersion>() else {
        return false;
    };
    match format!("python {}", spec).parse::<Requirement>() {
        Ok(req) => requirements_to_range(&req.constraints).contains(&v),
        Err(_) => false,
    }
}

/// Find an interpreter satisfying both `request` and `requires_python`.
pub fn find(request: &PythonRequest, requires_python: Option<&str>) -> Option<Interpreter> {
    let accept = |i: &Interpreter| {
        request.matches(i)
            && requires_python.is_none_or(|rp| version_satisfies(&i.version, rp))
            && !i.is_venv
    };

    if let PythonRequest::Path(path) = request {
        let resolved = if path.components().count() > 1 || path.is_absolute() {
            Some(path.clone())
        } else {
            which(path.as_os_str().to_str().unwrap_or_default())
        }?;
        let interp = probe(&resolved)?;
        // A venv interpreter given explicitly stands for its base.
        let interp = if interp.is_venv {
            probe(&interp.base_executable)?
        } else {
            interp
        };
        return accept(&interp).then_some(interp);
    }

    let mut seen = HashSet::new();
    for candidate in candidates() {
        let Some(interp) = probe(&candidate) else {
            continue;
        };
        let interp = if interp.is_venv {
            match probe(&interp.base_executable) {
                Some(base) => base,
                None => continue,
            }
        } else {
            interp
        };
        if !seen.insert(interp.executable.clone()) {
            continue;
        }
        if accept(&interp) {
            return Some(interp);
        }
    }
    None
}

/// Every interpreter vyp can find, deduplicated, in discovery order.
pub fn find_all() -> Vec<Interpreter> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for candidate in candidates() {
        let Some(interp) = probe(&candidate) else {
            continue;
        };
        if interp.is_venv {
            continue;
        }
        if seen.insert(interp.executable.clone()) {
            out.push(interp);
        }
    }
    out
}

/// Find a matching interpreter, installing a managed one when none exists
/// and downloads are allowed.
pub fn find_or_install(
    request: &PythonRequest,
    requires_python: Option<&str>,
    allow_download: bool,
) -> miette::Result<Interpreter> {
    if let Some(found) = find(request, requires_python) {
        return Ok(found);
    }
    if matches!(request, PythonRequest::Path(_)) {
        return Err(miette::miette!(
            "No usable Python interpreter at {:?}",
            request
        ));
    }
    if !allow_download || downloads_disabled() {
        return Err(miette::miette!(
            "No Python interpreter found matching {}{}. Install one with `vyp python install`.",
            describe_request(request),
            requires_python
                .map(|r| format!(" and requires-python {}", r))
                .unwrap_or_default()
        ));
    }
    eprintln!(
        "No Python matching {} found; downloading a managed interpreter...",
        describe_request(request)
    );
    download::install(request, requires_python)
}

pub fn describe_request(request: &PythonRequest) -> String {
    match request {
        PythonRequest::Any => "any version".to_string(),
        PythonRequest::Version {
            version,
            free_threaded,
        } => {
            format!("{}{}", version, if *free_threaded { "t" } else { "" })
        }
        PythonRequest::Specifier(s) => s.clone(),
        PythonRequest::Path(p) => p.display().to_string(),
    }
}

pub fn downloads_disabled() -> bool {
    std::env::var("VYP_NO_PYTHON_DOWNLOADS").is_ok_and(|v| v == "1" || v == "true")
}

/// Directory holding managed interpreters.
pub fn managed_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("VYP_PYTHON_INSTALL_DIR") {
        return PathBuf::from(dir);
    }
    let data = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local").join("share")
        });
    data.join("vyp").join("python")
}

/// Managed installs, newest version first.
pub fn managed_installs() -> Vec<(PathBuf, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(managed_dir()) else {
        return Vec::new();
    };
    let mut installs: Vec<(VypVersion, PathBuf, PathBuf)> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("cpython-"))
        .filter_map(|e| {
            let dir = e.path();
            let exe = managed_executable(&dir)?;
            let name = e.file_name().to_string_lossy().to_string();
            let version = name
                .strip_prefix("cpython-")?
                .split('-')
                .next()?
                .parse()
                .ok()?;
            Some((version, dir, exe))
        })
        .collect();
    installs.sort_by(|a, b| b.0.cmp(&a.0));
    installs
        .into_iter()
        .map(|(_, dir, exe)| (dir, exe))
        .collect()
}

fn managed_executable(install_dir: &Path) -> Option<PathBuf> {
    let candidates = if cfg!(windows) {
        vec![install_dir.join("python.exe")]
    } else {
        vec![install_dir.join("bin").join("python3")]
    };
    candidates.into_iter().find(|p| p.exists())
}

/// Candidate interpreter paths in discovery order.
fn candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = managed_installs().into_iter().map(|(_, exe)| exe).collect();

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| is_python_name(n))
                .collect();
            // Most specific first: python3.13 before python3 before python.
            names.sort_by_key(|n| std::cmp::Reverse(python_name_rank(n)));
            out.extend(names.into_iter().map(|n| dir.join(n)));
        }
    }

    let pyenv_root = std::env::var("PYENV_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".pyenv")
        });
    if let Ok(entries) = std::fs::read_dir(pyenv_root.join("versions")) {
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        dirs.reverse();
        out.extend(
            dirs.into_iter()
                .map(|d| d.join("bin").join("python3"))
                .filter(|p| p.exists()),
        );
    }
    out
}

fn is_python_name(name: &str) -> bool {
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    let Some(rest) = stem.strip_prefix("python") else {
        return false;
    };
    let rest = rest.strip_suffix('t').unwrap_or(rest);
    rest.is_empty()
        || rest == "3"
        || rest
            .strip_prefix("3.")
            .is_some_and(|m| !m.is_empty() && m.chars().all(|c| c.is_ascii_digit()))
}

fn python_name_rank(name: &str) -> (u32, u32) {
    let rest = name.trim_start_matches("python").trim_end_matches(".exe");
    let has_minor = rest.contains('.');
    let minor = rest
        .split('.')
        .nth(1)
        .and_then(|m| m.trim_end_matches('t').parse().ok())
        .unwrap_or(0);
    (
        if has_minor {
            2
        } else if rest == "3" {
            1
        } else {
            0
        },
        minor,
    )
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

const PROBE_SCRIPT: &str = r#"
import json, os, platform, sys, sysconfig
names = sysconfig.get_scheme_names()
scheme = "venv" if "venv" in names else ("nt" if os.name == "nt" else "posix_prefix")
base = "/__VYP_BASE__"
vars = {"base": base, "platbase": base, "installed_base": base, "installed_platbase": base, "userbase": base}
paths = sysconfig.get_paths(scheme=scheme, vars=vars)
def rel(p):
    p = p.replace("\\", "/")
    return p[len(base):].lstrip("/") if p.startswith(base) else p
print(json.dumps({
    "executable": os.path.realpath(sys.executable),
    "base_executable": os.path.realpath(getattr(sys, "_base_executable", sys.executable)),
    "version": platform.python_version(),
    "implementation": sys.implementation.name,
    "is_venv": sys.prefix != sys.base_prefix,
    "free_threaded": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
    "sys_platform": sys.platform,
    "machine": platform.machine(),
    "purelib": rel(paths["purelib"]),
    "scripts": rel(paths["scripts"]),
    "include": rel(paths["include"]),
}))
"#;

#[derive(Serialize, Deserialize)]
struct CachedProbe {
    mtime: u64,
    interpreter: Interpreter,
}

/// Probe an interpreter, using a cache keyed by path and modification time.
pub fn probe(path: &Path) -> Option<Interpreter> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // pyenv shims are scripts whose target changes with `pyenv global` and
    // `.python-version` without the shim changing, so never cache them.
    let cacheable = !path.to_string_lossy().contains("/shims/");
    let cache_file = cache_base_dir().join("interpreters").join(format!(
        "{}.json",
        hex_sha256(path.to_string_lossy().as_bytes())
    ));
    if cacheable {
        if let Some(cached) = std::fs::read_to_string(&cache_file)
            .ok()
            .and_then(|s| serde_json::from_str::<CachedProbe>(&s).ok())
            .filter(|c| c.mtime == mtime && c.interpreter.executable.exists())
        {
            return Some(cached.interpreter);
        }
    }

    let output = std::process::Command::new(path)
        .args(["-I", "-c", PROBE_SCRIPT])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut interpreter: Interpreter = serde_json::from_slice(&output.stdout).ok()?;
    interpreter.managed = interpreter.executable.starts_with(managed_dir())
        || std::fs::canonicalize(managed_dir())
            .is_ok_and(|d| interpreter.executable.starts_with(d));

    if cacheable {
        if let Some(dir) = cache_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            &cache_file,
            serde_json::to_string(&CachedProbe {
                mtime,
                interpreter: interpreter.clone(),
            })
            .unwrap_or_default(),
        );
    }
    Some(interpreter)
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interp(version: &str, free_threaded: bool) -> Interpreter {
        Interpreter {
            executable: PathBuf::from("/x/python"),
            base_executable: PathBuf::from("/x/python"),
            version: version.into(),
            implementation: "cpython".into(),
            is_venv: false,
            free_threaded,
            sys_platform: "linux".into(),
            machine: "x86_64".into(),
            purelib: "lib/python3.12/site-packages".into(),
            scripts: "bin".into(),
            include: "include/python3.12".into(),
            managed: false,
        }
    }

    #[test]
    fn parses_requests() {
        assert_eq!(PythonRequest::parse(""), PythonRequest::Any);
        assert_eq!(
            PythonRequest::parse("3.12"),
            PythonRequest::Version {
                version: "3.12".into(),
                free_threaded: false
            }
        );
        assert_eq!(
            PythonRequest::parse("python3.13t"),
            PythonRequest::Version {
                version: "3.13".into(),
                free_threaded: true
            }
        );
        assert_eq!(
            PythonRequest::parse(">=3.10"),
            PythonRequest::Specifier(">=3.10".into())
        );
        assert_eq!(
            PythonRequest::parse("/usr/bin/python3"),
            PythonRequest::Path("/usr/bin/python3".into())
        );
    }

    #[test]
    fn version_requests_match_prefixes() {
        let req = PythonRequest::parse("3.12");
        assert!(req.matches(&interp("3.12.7", false)));
        assert!(!req.matches(&interp("3.13.0", false)));
        assert!(!req.matches(&interp("3.12.7", true)));
        assert!(PythonRequest::parse("3.12.7").matches(&interp("3.12.7", false)));
        assert!(!PythonRequest::parse("3.12.6").matches(&interp("3.12.7", false)));
        assert!(PythonRequest::parse(">=3.11,<3.13").matches(&interp("3.12.1", false)));
    }

    #[test]
    fn recognizes_python_executable_names() {
        for ok in [
            "python",
            "python3",
            "python3.12",
            "python3.13t",
            "python3.exe",
        ] {
            assert!(is_python_name(ok), "{}", ok);
        }
        for bad in [
            "python3-config",
            "python3.12-config",
            "pythonw",
            "python3.x",
        ] {
            assert!(!is_python_name(bad), "{}", bad);
        }
    }

    #[test]
    fn probes_a_real_interpreter() {
        let Some(path) = which("python3") else { return };
        let interp = probe(&path).expect("probe python3");
        assert!(interp.version.starts_with('3'));
        assert!(interp.executable.is_absolute());
        assert!(
            interp.purelib.ends_with("site-packages"),
            "{}",
            interp.purelib
        );
    }
}
