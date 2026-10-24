//! A PEP 517 / PEP 660 build frontend.
//!
//! Builds happen in isolated build environments: a venv per distinct set of
//! `build-system.requires` (cached and reused), populated by vyp's own
//! resolver and installer with wheels only, so a build never recursively
//! builds its build dependencies. Backend hooks run through a small shim
//! (`backend_shim.py`) in the source tree, as PEP 517 specifies.

pub mod git;
pub mod sources;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use vyp_api::{MarkerEnvironment, Requirement};
use vyp_core::ResolverBuilder;

use crate::cache::startup::{StartupMode, StartupPolicy};
use crate::cache::venv::VenvLayout;
use crate::cache::wheel_cache::cache_base_dir;
use crate::cli::common::{install_lockfile, InstallOptions};
use crate::env::{self, VenvOptions};
use crate::lock::lockfile::LockFile;
use crate::python::{self, Interpreter};

const SHIM: &str = include_str!("backend_shim.py");
const DEFAULT_REQUIRES: &[&str] = &["setuptools>=40.8.0"];
const DEFAULT_BACKEND: &str = "setuptools.build_meta:__legacy__";

/// `[build-system]` of a source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSystem {
    pub requires: Vec<String>,
    pub backend: String,
    pub backend_path: Vec<String>,
}

impl BuildSystem {
    pub fn read(source: &Path) -> miette::Result<Self> {
        let pyproject = source.join("pyproject.toml");
        let defaults = Self {
            requires: DEFAULT_REQUIRES.iter().map(|s| s.to_string()).collect(),
            backend: DEFAULT_BACKEND.to_string(),
            backend_path: Vec::new(),
        };
        let Ok(content) = std::fs::read_to_string(&pyproject) else {
            if source.join("setup.py").exists() {
                return Ok(defaults);
            }
            return Err(miette::miette!(
                "{} has no pyproject.toml or setup.py",
                source.display()
            ));
        };
        let doc: toml::Value = toml::from_str(&content)
            .map_err(|e| miette::miette!("Failed to parse {}: {}", pyproject.display(), e))?;
        let Some(table) = doc.get("build-system") else {
            return Ok(defaults);
        };
        let strings = |key: &str| -> Vec<String> {
            table
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        // PEP 517: no build-backend means the legacy setuptools backend.
        match table.get("build-backend").and_then(|v| v.as_str()) {
            Some(backend) => Ok(Self {
                requires: strings("requires"),
                backend: backend.to_string(),
                backend_path: strings("backend-path"),
            }),
            None => Ok(Self {
                requires: if table.get("requires").is_some() {
                    strings("requires")
                } else {
                    defaults.requires
                },
                ..defaults
            }),
        }
    }
}

/// Builds source trees for one target interpreter.
#[derive(Clone)]
pub struct Frontend {
    base: Interpreter,
    index_url: String,
    client: reqwest::Client,
}

impl Frontend {
    /// A frontend building for the interpreter at `python` (a venv's or a
    /// base interpreter); build environments use its base interpreter.
    pub fn for_python(
        python: &Path,
        index_url: &str,
        client: reqwest::Client,
    ) -> miette::Result<Self> {
        let interp = python::probe(python)
            .ok_or_else(|| miette::miette!("Cannot run {} to build packages", python.display()))?;
        let base = if interp.is_venv {
            python::probe(&interp.base_executable).ok_or_else(|| {
                miette::miette!(
                    "Base interpreter {} doesn't run",
                    interp.base_executable.display()
                )
            })?
        } else {
            interp
        };
        Ok(Self {
            base,
            index_url: index_url.to_string(),
            client,
        })
    }

    /// Identifies the interpreter builds target, for caching built wheels.
    pub fn interpreter_key(&self) -> String {
        format!("{} {}", self.base.executable.display(), self.base.version)
    }

    /// The build environment for a set of requirements, created on first use.
    fn env_for(&self, requires: &[String]) -> miette::Result<VenvLayout> {
        let mut sorted: Vec<String> = requires.iter().map(|r| r.trim().to_string()).collect();
        sorted.sort();
        sorted.dedup();
        let key = python::hex_sha256(
            format!(
                "{}\n{}\n{}",
                self.base.executable.display(),
                self.index_url,
                sorted.join("\n")
            )
            .as_bytes(),
        );
        let dir = cache_base_dir().join("build-envs").join(&key[..24]);
        let done = dir.join(".vyp-complete");
        if done.exists() {
            if let Ok(layout) = VenvLayout::from_root(&dir) {
                return Ok(layout);
            }
        }

        let layout = env::create(
            &dir,
            &self.base,
            &VenvOptions {
                clear: true,
                ..Default::default()
            },
        )?;
        if !sorted.is_empty() {
            let reqs: Vec<Requirement> = sorted
                .iter()
                .map(|r| {
                    r.parse()
                        .map_err(|e| miette::miette!("Invalid build requirement '{}': {}", r, e))
                })
                .collect::<miette::Result<_>>()?;
            let marker_env = MarkerEnvironment::detect_with(&layout.python_exe.to_string_lossy());
            let provider = vyp_index::PyPIMetadataProvider::with_name_env_client(
                "pypi",
                &self.index_url,
                None,
                Some(marker_env),
                Some(self.client.clone()),
            )
            .with_resolution_options(vyp_index::SdistPolicy::Never, Default::default());
            let result = ResolverBuilder::new()
                .with_provider(Box::new(provider))
                .add_dependencies(reqs)
                .resolve()
                .map_err(|e| {
                    miette::miette!(
                        "Cannot resolve build requirements {}:\n{}",
                        sorted.join(", "),
                        e
                    )
                })?;
            let lock = LockFile::from_resolution(&result);
            install_lockfile(
                &lock,
                &layout,
                &InstallOptions {
                    default_index: Some(self.index_url.clone()),
                    client: Some(self.client.clone()),
                    // Build tools such as setuptools ship startup files; build
                    // environments are disposable and never run user code.
                    startup: StartupPolicy {
                        mode: StartupMode::Allow,
                        ..Default::default()
                    },
                    quiet: true,
                    ..Default::default()
                },
            )?;
        }
        std::fs::write(&done, b"")
            .map_err(|e| miette::miette!("Failed to write {}: {}", done.display(), e))?;
        Ok(layout)
    }

    /// Run a backend hook. `Ok(None)` means the backend doesn't define it.
    fn hook(
        &self,
        env: &VenvLayout,
        source: &Path,
        system: &BuildSystem,
        hook: &str,
        args: serde_json::Value,
    ) -> miette::Result<Option<serde_json::Value>> {
        let work = scratch_dir("hook")?;
        let shim = work.join("backend_shim.py");
        let result_path = work.join("result.json");
        std::fs::write(&shim, SHIM)
            .map_err(|e| miette::miette!("Failed to write build shim: {}", e))?;

        let verbose = std::env::var("VYP_BUILD_VERBOSE").is_ok_and(|v| v == "1");
        let output = std::process::Command::new(&env.python_exe)
            .arg(&shim)
            .arg(hook)
            .arg(&system.backend)
            .arg(serde_json::to_string(&system.backend_path).unwrap_or_else(|_| "[]".into()))
            .arg(args.to_string())
            .arg(&result_path)
            .current_dir(source)
            .env("PYTHONNOUSERSITE", "1")
            .env_remove("PYTHONPATH")
            .stdin(std::process::Stdio::null())
            .stdout(if verbose {
                std::process::Stdio::inherit()
            } else {
                std::process::Stdio::piped()
            })
            .stderr(if verbose {
                std::process::Stdio::inherit()
            } else {
                std::process::Stdio::piped()
            })
            .output()
            .map_err(|e| miette::miette!("Failed to run {}: {}", env.python_exe.display(), e))?;

        let result: serde_json::Value = std::fs::read_to_string(&result_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(&work);

        if result["ok"] == true {
            return Ok(Some(result["value"].clone()));
        }
        if result["missing"] == true {
            return Ok(None);
        }
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let tail: Vec<&str> = log.lines().rev().take(25).collect();
        Err(miette::miette!(
            "Build backend {} failed in `{}` for {}:\n{}{}",
            system.backend,
            hook,
            source.display(),
            result["error"].as_str().unwrap_or(""),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        ))
    }

    /// The build environment with the backend's dynamic requirements added.
    fn prepared_env(
        &self,
        source: &Path,
        system: &BuildSystem,
        editable: bool,
    ) -> miette::Result<VenvLayout> {
        let env = self.env_for(&system.requires)?;
        let get_requires = if editable {
            "get_requires_for_build_editable"
        } else {
            "get_requires_for_build_wheel"
        };
        let extra: Vec<String> = self
            .hook(
                &env,
                source,
                system,
                get_requires,
                serde_json::json!({"args": [{}]}),
            )?
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if extra.is_empty() {
            return Ok(env);
        }
        let mut all = system.requires.clone();
        all.extend(extra);
        self.env_for(&all)
    }

    /// The core metadata (`METADATA` text) a build of `source` would produce.
    pub fn metadata(&self, source: &Path) -> miette::Result<String> {
        let system = BuildSystem::read(source)?;
        let env = self.prepared_env(source, &system, false)?;
        let out = scratch_dir("metadata")?;
        let prepared = self.hook(
            &env,
            source,
            &system,
            "prepare_metadata_for_build_wheel",
            serde_json::json!({"args": [out.to_string_lossy(), {}]}),
        )?;
        let metadata = match prepared.as_ref().and_then(|v| v.as_str()) {
            Some(dist_info) => std::fs::read_to_string(out.join(dist_info).join("METADATA"))
                .map_err(|e| miette::miette!("Backend produced no METADATA: {}", e)),
            // Optional hook (PEP 517): build the wheel and read it instead.
            None => {
                let wheel = self.build_in(&env, source, &system, false, &out)?;
                read_wheel_metadata(&wheel)
            }
        };
        let _ = std::fs::remove_dir_all(&out);
        metadata
    }

    /// Build a wheel (or, with `editable`, a PEP 660 editable wheel) of
    /// `source` into `out_dir`.
    pub fn build(&self, source: &Path, editable: bool, out_dir: &Path) -> miette::Result<PathBuf> {
        let system = BuildSystem::read(source)?;
        let env = self.prepared_env(source, &system, editable)?;
        self.build_in(&env, source, &system, editable, out_dir)
    }

    fn build_in(
        &self,
        env: &VenvLayout,
        source: &Path,
        system: &BuildSystem,
        editable: bool,
        out_dir: &Path,
    ) -> miette::Result<PathBuf> {
        std::fs::create_dir_all(out_dir)
            .map_err(|e| miette::miette!("Failed to create {}: {}", out_dir.display(), e))?;
        let hook = if editable {
            "build_editable"
        } else {
            "build_wheel"
        };
        let name = self
            .hook(
                env,
                source,
                system,
                hook,
                serde_json::json!({"args": [out_dir.to_string_lossy(), {}]}),
            )?
            .ok_or_else(|| {
                if editable {
                    miette::miette!(
                        "Build backend {} doesn't support editable installs (PEP 660)",
                        system.backend
                    )
                } else {
                    miette::miette!("Build backend {} has no build_wheel hook", system.backend)
                }
            })?;
        let name = name
            .as_str()
            .ok_or_else(|| miette::miette!("Backend returned no wheel name"))?;
        Ok(out_dir.join(name))
    }
}

/// METADATA from a built wheel.
pub fn read_wheel_metadata(wheel: &Path) -> miette::Result<String> {
    let file = std::fs::File::open(wheel)
        .map_err(|e| miette::miette!("Cannot open {}: {}", wheel.display(), e))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| miette::miette!("Bad wheel {}: {}", wheel.display(), e))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| miette::miette!("Bad wheel {}: {}", wheel.display(), e))?;
        let name = entry.name().to_string();
        if name.ends_with(".dist-info/METADATA") && name.matches('/').count() == 1 {
            let mut s = String::new();
            entry
                .read_to_string(&mut s)
                .map_err(|e| miette::miette!("Bad METADATA in {}: {}", wheel.display(), e))?;
            return Ok(s);
        }
    }
    Err(miette::miette!("No METADATA in {}", wheel.display()))
}

/// Unpack an sdist into the cache and return its source tree.
pub fn unpack_sdist(path: &Path) -> miette::Result<PathBuf> {
    let bytes = std::fs::read(path)
        .map_err(|e| miette::miette!("Cannot read {}: {}", path.display(), e))?;
    let key = python::hex_sha256(&bytes);
    let dest = cache_base_dir().join("sdist-src").join(&key[..24]);
    if !dest.join(".vyp-complete").exists() {
        let _ = std::fs::remove_dir_all(&dest);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.ends_with(".zip") {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(&bytes))
                .map_err(|e| miette::miette!("Bad sdist {}: {}", name, e))?;
            // `extract` refuses entries that escape the destination.
            zip.extract(&dest)
                .map_err(|e| miette::miette!("Cannot unpack {}: {}", name, e))?;
        } else {
            crate::python::download::unpack_tar_gz(&bytes, &dest)?;
        }
        std::fs::write(dest.join(".vyp-complete"), b"").map_err(|e| miette::miette!("{}", e))?;
    }
    // An sdist holds one top-level `<name>-<version>/` directory.
    let mut dirs = std::fs::read_dir(&dest)
        .map_err(|e| miette::miette!("{}", e))?
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.path());
    match (dirs.next(), dirs.next()) {
        (Some(dir), None) => Ok(dir),
        _ => Err(miette::miette!(
            "Unexpected layout in sdist {}",
            path.display()
        )),
    }
}

/// A fresh scratch directory under the cache.
pub fn scratch_dir(label: &str) -> miette::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = cache_base_dir().join("tmp").join(format!(
        "{}-{}-{}-{}",
        label,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| miette::miette!("Failed to create {}: {}", dir.display(), e))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pyproject: Option<&str>, setup_py: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "vyp-bs-{}-{}",
            std::process::id(),
            pyproject.map(|p| p.len()).unwrap_or(0) + setup_py as usize
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(p) = pyproject {
            std::fs::write(dir.join("pyproject.toml"), p).unwrap();
        }
        if setup_py {
            std::fs::write(dir.join("setup.py"), "").unwrap();
        }
        dir
    }

    #[test]
    fn reads_build_system_with_pep517_defaults() {
        let explicit = tree(
            Some(
                "[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
            ),
            false,
        );
        let bs = BuildSystem::read(&explicit).unwrap();
        assert_eq!(bs.requires, vec!["hatchling"]);
        assert_eq!(bs.backend, "hatchling.build");

        let legacy = tree(None, true);
        let bs = BuildSystem::read(&legacy).unwrap();
        assert_eq!(bs.backend, DEFAULT_BACKEND);
        assert_eq!(bs.requires, vec!["setuptools>=40.8.0"]);

        let requires_only = tree(
            Some("[build-system]\nrequires = [\"setuptools>=61\", \"wheel\"]\n"),
            false,
        );
        let bs = BuildSystem::read(&requires_only).unwrap();
        assert_eq!(bs.backend, DEFAULT_BACKEND);
        assert_eq!(bs.requires, vec!["setuptools>=61", "wheel"]);

        assert!(BuildSystem::read(&tree(None, false)).is_err());
        for d in [explicit, legacy, requires_only] {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
