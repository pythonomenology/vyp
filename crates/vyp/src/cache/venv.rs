use std::path::{Path, PathBuf};

/// The install scheme paths for a virtual environment, mirroring the sysconfig
/// "venv" scheme. Wheels install their importable code into `site_packages`
/// and relocate their `*.data/{scripts,data,headers,purelib,platlib}` payloads
/// to the matching directory here.
#[derive(Debug, Clone)]
pub struct VenvLayout {
    pub root: PathBuf,
    pub site_packages: PathBuf,
    /// `bin/` on POSIX, `Scripts/` on Windows.
    pub scripts: PathBuf,
    /// `include/` (headers scheme target).
    pub include: PathBuf,
    /// Path to the venv's Python interpreter (used for script shebangs).
    pub python_exe: PathBuf,
    /// "X.Y" python version derived from the lib directory. Retained for
    /// bytecode/headers paths and diagnostics.
    #[allow(dead_code)]
    pub python_version: String,
    /// Created with `relocatable = true` in `pyvenv.cfg`: scripts locate the
    /// interpreter relative to themselves instead of by absolute path.
    pub relocatable: bool,
}

impl VenvLayout {
    /// Discover the layout of a virtual environment, auto-detecting from
    /// `--venv`, `$VIRTUAL_ENV`, or `./.venv` when not given explicitly.
    pub fn discover(venv: Option<&Path>) -> miette::Result<Self> {
        let root = resolve_venv_root(venv)?;
        // Canonicalize so script shebangs and RECORD paths are absolute and
        // remain valid regardless of the cwd at install time.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Self::from_root(&root)
    }

    pub fn from_root(root: &Path) -> miette::Result<Self> {
        if cfg!(windows) {
            let site_packages = root.join("Lib").join("site-packages");
            if !site_packages.exists() {
                return Err(miette::miette!(
                    "site-packages not found at {}",
                    site_packages.display()
                ));
            }
            return Ok(Self {
                root: root.to_path_buf(),
                site_packages,
                scripts: root.join("Scripts"),
                include: root.join("Include"),
                python_exe: root.join("Scripts").join("python.exe"),
                python_version: detect_windows_python_version(root),
                relocatable: read_pyvenv_cfg(root)
                    .get("relocatable")
                    .is_some_and(|v| v == "true"),
            });
        }

        let lib_dir = root.join("lib");
        if !lib_dir.exists() {
            return Err(miette::miette!(
                "Invalid virtual environment: {} has no lib/ directory",
                root.display()
            ));
        }

        let python_dir = std::fs::read_dir(&lib_dir)
            .map_err(|e| miette::miette!("Cannot read {}: {}", lib_dir.display(), e))?
            .filter_map(|e| e.ok())
            .find(|e| e.file_name().to_string_lossy().starts_with("python"))
            .ok_or_else(|| miette::miette!("No python directory found in {}", lib_dir.display()))?;

        let py_name = python_dir.file_name().to_string_lossy().to_string();
        let python_version = py_name.trim_start_matches("python").to_string();

        let site = python_dir.path().join("site-packages");
        if !site.exists() {
            return Err(miette::miette!(
                "site-packages not found at {}",
                site.display()
            ));
        }

        Ok(Self {
            root: root.to_path_buf(),
            site_packages: site,
            scripts: root.join("bin"),
            include: root.join("include"),
            python_exe: root.join("bin").join("python"),
            python_version,
            relocatable: read_pyvenv_cfg(root)
                .get("relocatable")
                .is_some_and(|v| v == "true"),
        })
    }

    /// The interpreter line(s) that start a generated or rewritten script.
    ///
    /// A relocatable venv, or an interpreter path a kernel can't put in a
    /// `#!` line (too long, or containing spaces), gets a `/bin/sh` header
    /// that re-executes the script under the right Python.
    pub fn script_header(&self) -> String {
        if self.relocatable {
            return "#!/bin/sh\n'''exec' \"$(dirname -- \"$(realpath -- \"$0\")\")\"/'python' \"$0\" \"$@\"\n' '''\n".to_string();
        }
        let python = self.python_exe.display().to_string();
        if python.len() > 127 || python.contains(' ') {
            return format!(
                "#!/bin/sh\n'''exec' '{}' \"$0\" \"$@\"\n' '''\n",
                python.replace('\'', "'\\''")
            );
        }
        format!("#!{}\n", python)
    }

    /// Map a wheel `*.data/<scheme>/...` payload to its destination directory.
    pub fn data_scheme_target(&self, scheme: &str) -> PathBuf {
        match scheme {
            "purelib" | "platlib" => self.site_packages.clone(),
            "scripts" => self.scripts.clone(),
            "headers" => self.include.clone(),
            // The `data` scheme installs relative to the environment root.
            "data" => self.root.clone(),
            // Unknown scheme: fall back to site-packages to avoid losing files.
            _ => self.site_packages.clone(),
        }
    }
}

/// `key = value` pairs from a venv's `pyvenv.cfg`.
pub fn read_pyvenv_cfg(root: &Path) -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(root.join("pyvenv.cfg"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn resolve_venv_root(venv: Option<&Path>) -> miette::Result<PathBuf> {
    if let Some(p) = venv {
        return Ok(p.to_path_buf());
    }
    if let Ok(virtual_env) = std::env::var("VIRTUAL_ENV") {
        return Ok(PathBuf::from(virtual_env));
    }
    let local = PathBuf::from(".venv");
    if local.exists() {
        return Ok(local);
    }
    Err(miette::miette!(
        "No virtual environment found. Use --venv to specify one, \
         activate one, or create .venv in the current directory."
    ))
}

fn detect_windows_python_version(root: &Path) -> String {
    // Best-effort: read pyvenv.cfg `version = X.Y.Z`.
    if let Ok(cfg) = std::fs::read_to_string(root.join("pyvenv.cfg")) {
        for line in cfg.lines() {
            if let Some(v) = line.split_once('=').filter(|(k, _)| k.trim() == "version") {
                let parts: Vec<&str> = v.1.trim().split('.').collect();
                if parts.len() >= 2 {
                    return format!("{}.{}", parts[0], parts[1]);
                }
            }
        }
    }
    "3".to_string()
}
