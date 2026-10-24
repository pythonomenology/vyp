//! Recording what a virtual environment was built from, and noticing when
//! that stops being true.
//!
//! Follows the shape of the "Recording virtual environment provenance"
//! pre-PEP (discuss.python.org/t/104777): a top-level `venv-info/` directory
//! holding a `MANAGER` file and a `pylock.snapshot.toml` of what the managing
//! tool installed, plus tool-specific files under `venv-info/vyp/`.
//!
//! A venv goes stale when its base interpreter is removed or replaced, when it
//! is copied to a machine with a different OS or CPU, or when another tool
//! changes it. `check` reports each of those, and installs refuse to touch an
//! environment whose interpreter is gone or whose platform has changed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::VenvOptions;
use crate::cache::installed::installed_distributions;
use crate::cache::venv::{read_pyvenv_cfg, VenvLayout};
use crate::lock::lockfile::LockFile;
use crate::python::Interpreter;

pub const DIR: &str = "venv-info";
const MANAGER: &str = "vyp";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct EnvironmentRecord {
    pub vyp_version: String,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
    pub relocatable: bool,
    pub interpreter: InterpreterRecord,
    pub host: HostRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct InterpreterRecord {
    pub executable: PathBuf,
    pub version: String,
    pub implementation: String,
    pub free_threaded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostRecord {
    pub os: String,
    pub arch: String,
}

impl HostRecord {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }
}

fn info_dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

fn record_path(root: &Path) -> PathBuf {
    info_dir(root).join("vyp").join("environment.toml")
}

fn now_rfc3339() -> String {
    vyp_index::timestamp::format_rfc3339(vyp_index::timestamp::now())
}

fn write(path: &Path, content: &str) -> miette::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| miette::miette!("Failed to create {}: {}", dir.display(), e))?;
    }
    std::fs::write(path, content)
        .map_err(|e| miette::miette!("Failed to write {}: {}", path.display(), e))
}

/// Record the interpreter and host a new environment was created for.
pub fn record_creation(
    root: &Path,
    interp: &Interpreter,
    opts: &VenvOptions,
) -> miette::Result<()> {
    write(&info_dir(root).join("MANAGER"), &format!("{}\n", MANAGER))?;
    let record = EnvironmentRecord {
        vyp_version: env!("CARGO_PKG_VERSION").to_string(),
        created: now_rfc3339(),
        updated: None,
        relocatable: opts.relocatable,
        interpreter: InterpreterRecord {
            executable: interp.base_executable.clone(),
            version: interp.version.clone(),
            implementation: interp.implementation.clone(),
            free_threaded: interp.free_threaded,
        },
        host: HostRecord::current(),
    };
    write(
        &record_path(root),
        &toml::to_string_pretty(&record).unwrap_or_default(),
    )
}

pub fn read_record(root: &Path) -> Option<EnvironmentRecord> {
    toml::from_str(&std::fs::read_to_string(record_path(root)).ok()?).ok()
}

pub fn manager(root: &Path) -> Option<String> {
    std::fs::read_to_string(info_dir(root).join("MANAGER"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// After an install or sync: claim the environment and snapshot what the
/// lock put there.
pub fn record_install(layout: &VenvLayout, lock: &LockFile) -> miette::Result<()> {
    let root = &layout.root;
    write(&info_dir(root).join("MANAGER"), &format!("{}\n", MANAGER))?;
    let snapshot = toml::to_string_pretty(lock)
        .map_err(|e| miette::miette!("Failed to serialize lock snapshot: {}", e))?;
    write(&info_dir(root).join("pylock.snapshot.toml"), &snapshot)?;

    // Environments created by other tools get a record on first install,
    // from what pyvenv.cfg says.
    let mut record = read_record(root).unwrap_or_else(|| record_from_pyvenv_cfg(root));
    record.updated = Some(now_rfc3339());
    record.vyp_version = env!("CARGO_PKG_VERSION").to_string();
    write(
        &record_path(root),
        &toml::to_string_pretty(&record).unwrap_or_default(),
    )
}

fn record_from_pyvenv_cfg(root: &Path) -> EnvironmentRecord {
    let cfg = read_pyvenv_cfg(root);
    let executable = cfg
        .get("executable")
        .map(PathBuf::from)
        .or_else(|| cfg.get("home").map(|h| PathBuf::from(h).join("python3")))
        .unwrap_or_default();
    EnvironmentRecord {
        vyp_version: env!("CARGO_PKG_VERSION").to_string(),
        created: now_rfc3339(),
        updated: None,
        relocatable: cfg.get("relocatable").is_some_and(|v| v == "true"),
        interpreter: InterpreterRecord {
            executable,
            version: cfg
                .get("version")
                .or_else(|| cfg.get("version_info"))
                .cloned()
                .unwrap_or_default(),
            implementation: cfg
                .get("implementation")
                .cloned()
                .unwrap_or_else(|| "cpython".into())
                .to_lowercase(),
            free_threaded: false,
        },
        host: HostRecord::current(),
    }
}

/// One way an environment no longer matches what it was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// The base interpreter the venv links to no longer exists.
    InterpreterMissing(PathBuf),
    /// The base interpreter now reports a different version.
    InterpreterChanged {
        executable: PathBuf,
        was: String,
        now: String,
    },
    /// The venv was created on a different OS or CPU architecture.
    HostChanged {
        was: HostRecord,
        now: HostRecord,
    },
    /// Another tool claims the environment.
    OtherManager(String),
    /// Installed packages differ from the last vyp snapshot.
    PackageMissing {
        name: String,
        version: String,
    },
    PackageChanged {
        name: String,
        snapshot: String,
        installed: String,
    },
    PackageAdded {
        name: String,
        version: String,
    },
}

impl Drift {
    /// Drift that makes installing into the environment unsafe.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Self::InterpreterMissing(_)
                | Self::InterpreterChanged { .. }
                | Self::HostChanged { .. }
        )
    }

    pub fn describe(&self) -> String {
        match self {
            Self::InterpreterMissing(p) => {
                format!("base interpreter {} no longer exists", p.display())
            }
            Self::InterpreterChanged {
                executable,
                was,
                now,
            } => format!(
                "base interpreter {} changed from Python {} to {}",
                executable.display(),
                was,
                now
            ),
            Self::HostChanged { was, now } => format!(
                "created on {}/{} but now running on {}/{}",
                was.os, was.arch, now.os, now.arch
            ),
            Self::OtherManager(m) => format!("managed by {} (venv-info/MANAGER)", m),
            Self::PackageMissing { name, version } => {
                format!("{}=={} is in the snapshot but not installed", name, version)
            }
            Self::PackageChanged {
                name,
                snapshot,
                installed,
            } => {
                format!(
                    "{} is {} but the snapshot has {}",
                    name, installed, snapshot
                )
            }
            Self::PackageAdded { name, version } => {
                format!("{}=={} was installed outside vyp", name, version)
            }
        }
    }
}

/// Everything that no longer matches the environment's records. Package
/// drift is only reported when vyp has a snapshot to compare against.
pub fn check(layout: &VenvLayout) -> Vec<Drift> {
    let root = &layout.root;
    let mut drift = Vec::new();
    let record = read_record(root).unwrap_or_else(|| record_from_pyvenv_cfg(root));

    let exe = &record.interpreter.executable;
    if !exe.as_os_str().is_empty() {
        if !exe.exists() {
            drift.push(Drift::InterpreterMissing(exe.clone()));
        } else if let Some(now) = crate::python::probe(exe) {
            if !record.interpreter.version.is_empty() && now.version != record.interpreter.version {
                drift.push(Drift::InterpreterChanged {
                    executable: exe.clone(),
                    was: record.interpreter.version.clone(),
                    now: now.version,
                });
            }
        }
    }

    let host = HostRecord::current();
    if record.host != host {
        drift.push(Drift::HostChanged {
            was: record.host.clone(),
            now: host,
        });
    }

    if let Some(m) = manager(root).filter(|m| m != MANAGER) {
        drift.push(Drift::OtherManager(m));
    }

    if let Some(snapshot) = std::fs::read_to_string(info_dir(root).join("pylock.snapshot.toml"))
        .ok()
        .and_then(|s| toml::from_str::<LockFile>(&s).ok())
    {
        let expected: BTreeMap<String, String> = snapshot
            .packages
            .iter()
            .map(|p| (vyp_api::normalize_package_name(&p.name), p.version.clone()))
            .collect();
        let installed: BTreeMap<String, String> = installed_distributions(layout)
            .into_iter()
            .map(|d| (d.name, d.version))
            .collect();
        for (name, version) in &expected {
            match installed.get(name) {
                None => drift.push(Drift::PackageMissing {
                    name: name.clone(),
                    version: version.clone(),
                }),
                Some(v) if v != version => drift.push(Drift::PackageChanged {
                    name: name.clone(),
                    snapshot: version.clone(),
                    installed: v.clone(),
                }),
                _ => {}
            }
        }
        for (name, version) in &installed {
            if !expected.contains_key(name)
                && !matches!(name.as_str(), "pip" | "setuptools" | "wheel")
            {
                drift.push(Drift::PackageAdded {
                    name: name.clone(),
                    version: version.clone(),
                });
            }
        }
    }
    drift
}

/// Before installing: refuse a stale environment, warn about another manager.
pub fn preflight(layout: &VenvLayout) -> miette::Result<()> {
    let drift = check(layout);
    let fatal: Vec<String> = drift
        .iter()
        .filter(|d| d.is_fatal())
        .map(Drift::describe)
        .collect();
    if !fatal.is_empty() {
        return Err(miette::miette!(
            "The virtual environment at {} is stale:\n  {}\nRecreate it with `vyp venv --clear {}`.",
            layout.root.display(),
            fatal.join("\n  "),
            layout.root.display()
        ));
    }
    for d in &drift {
        if let Drift::OtherManager(m) = d {
            eprintln!(
                "warning: {} is managed by {}; vyp will take it over (venv-info/MANAGER)",
                layout.root.display(),
                m
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vyp-prov-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[cfg(unix)]
    #[test]
    fn detects_missing_interpreter_host_change_and_other_manager() {
        let Some(interp) = crate::python::find(&crate::python::PythonRequest::Any, None) else {
            return;
        };
        let dir = scratch("drift");
        let layout = super::super::create(&dir, &interp, &VenvOptions::default()).unwrap();
        assert!(check(&layout).is_empty(), "{:?}", check(&layout));

        let mut record = read_record(&dir).unwrap();
        record.interpreter.executable = dir.join("gone/python3");
        record.host.arch = "sparc".into();
        write(
            &record_path(&dir),
            &toml::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        write(&info_dir(&dir).join("MANAGER"), "other-tool\n").unwrap();

        let drift = check(&layout);
        assert!(drift
            .iter()
            .any(|d| matches!(d, Drift::InterpreterMissing(_))));
        assert!(drift.iter().any(|d| matches!(d, Drift::HostChanged { .. })));
        assert!(drift.contains(&Drift::OtherManager("other-tool".into())));
        assert!(preflight(&layout).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
