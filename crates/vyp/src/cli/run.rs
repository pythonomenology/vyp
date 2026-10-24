//! `vyp run`: run a PEP 723 script in a cached environment built from its
//! inline metadata, or a command inside the project environment.
//!
//! A script may also carry its lock in a `# /// pylock` block (written by
//! `vyp lock --script`, following the "Locking a PEP 723 single-file script"
//! pre-PEP), in which case `vyp run` installs exactly that lock instead of
//! resolving.

use std::path::{Path, PathBuf};

use clap::Args;

use crate::cache::venv::VenvLayout;
use crate::cache::wheel_cache::cache_base_dir;
use crate::config::settings::VypConfig;
use crate::env::{self, VenvOptions};
use crate::lock::lockfile::LockFile;
use crate::python::{self, PythonRequest};

use super::common::{install_lockfile, resolve_from_config, InstallOptions, ResolveOut};

#[derive(Args)]
pub struct RunArgs {
    /// A script (`script.py`) or a command to run in the project environment.
    pub target: String,

    /// Arguments passed to the script or command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,

    /// Extra requirements for a script run, on top of its inline metadata.
    #[arg(long = "with")]
    pub with: Vec<String>,

    /// Interpreter for a script environment.
    #[arg(short, long)]
    pub python: Option<String>,

    /// Path to pyproject.toml (for commands run in the project environment).
    #[arg(long, default_value = "pyproject.toml")]
    pub project: PathBuf,
}

/// The parsed inline metadata blocks of a script.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ScriptMetadata {
    /// The `script` block as TOML.
    pub script: Option<toml::Table>,
    /// The `pylock` block as TOML.
    pub pylock: Option<String>,
}

/// Parse PEP 723 blocks: `# /// TYPE` ... `# ///`, each content line `#`
/// or `# ` followed by the text. A type may appear once.
pub fn parse_blocks(source: &str) -> miette::Result<Vec<(String, String)>> {
    let mut blocks: Vec<(String, String)> = Vec::new();
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        let Some(kind) = line.strip_prefix("# /// ") else {
            continue;
        };
        let kind = kind.trim_end();
        if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            continue;
        }
        let mut content = String::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if inner.trim_end() == "# ///" {
                closed = true;
                break;
            }
            let text = if inner == "#" {
                ""
            } else if let Some(t) = inner.strip_prefix("# ") {
                t
            } else {
                return Err(miette::miette!(
                    "Malformed `# /// {}` block: line {:?} isn't a comment",
                    kind,
                    inner
                ));
            };
            content.push_str(text);
            content.push('\n');
        }
        if !closed {
            return Err(miette::miette!("Unclosed `# /// {}` block", kind));
        }
        if blocks.iter().any(|(k, _)| k == kind) {
            return Err(miette::miette!("More than one `# /// {}` block", kind));
        }
        blocks.push((kind.to_string(), content));
    }
    Ok(blocks)
}

pub fn read_metadata(script: &Path) -> miette::Result<ScriptMetadata> {
    let source = std::fs::read_to_string(script)
        .map_err(|e| miette::miette!("Cannot read {}: {}", script.display(), e))?;
    let mut meta = ScriptMetadata::default();
    for (kind, content) in parse_blocks(&source)? {
        match kind.as_str() {
            "script" => {
                meta.script = Some(toml::from_str(&content).map_err(|e| {
                    miette::miette!(
                        "Invalid `# /// script` metadata in {}: {}",
                        script.display(),
                        e
                    )
                })?)
            }
            "pylock" => meta.pylock = Some(content),
            _ => {}
        }
    }
    Ok(meta)
}

/// A `VypConfig` for a script's metadata: its dependencies and
/// `requires-python` as a project, and its `[tool.vyp]` table as settings.
pub fn script_config(
    script: &Path,
    block: &toml::Table,
    with: &[String],
) -> miette::Result<VypConfig> {
    let mut project = toml::Table::new();
    project.insert("name".into(), "script".into());
    project.insert("version".into(), "0".into());
    let mut deps: Vec<toml::Value> = block
        .get("dependencies")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    deps.extend(with.iter().map(|w| toml::Value::String(w.clone())));
    project.insert("dependencies".into(), toml::Value::Array(deps));
    if let Some(rp) = block.get("requires-python") {
        project.insert("requires-python".into(), rp.clone());
    }
    let mut doc = toml::Table::new();
    doc.insert("project".into(), toml::Value::Table(project));
    if let Some(tool) = block.get("tool") {
        doc.insert("tool".into(), tool.clone());
    }
    let text = toml::to_string(&doc).map_err(|e| miette::miette!("{}", e))?;
    let mut config = VypConfig::from_str(&text)?;
    config.project_dir = script
        .parent()
        .map(Path::to_path_buf)
        .filter(|p| !p.as_os_str().is_empty());
    Ok(config)
}

pub fn run(args: RunArgs) -> miette::Result<()> {
    let target = PathBuf::from(&args.target);
    let is_script = target.extension().is_some_and(|e| e == "py" || e == "pyw") && target.is_file();
    if is_script {
        let python = script_environment(&target, &args)?;
        exec(
            &python,
            std::iter::once(target.as_os_str().to_owned()).chain(args.args.iter().map(Into::into)),
            None,
        )
    } else {
        // A command in the project environment.
        let config = if args.project.exists() {
            VypConfig::from_file(&args.project)?
        } else {
            VypConfig::default()
        };
        let layout = super::common::target_venv(&config, None)?;
        let program = layout.scripts.join(&args.target);
        let program = if program.exists() {
            program
        } else {
            PathBuf::from(&args.target)
        };
        exec(&program, args.args.iter().map(Into::into), Some(&layout))
    }
}

/// The interpreter of a script's environment, created and populated if
/// needed. Environments are cached by the script's metadata.
fn script_environment(script: &Path, args: &RunArgs) -> miette::Result<PathBuf> {
    let meta = read_metadata(script)?;
    let Some(block) = meta
        .script
        .clone()
        .or_else(|| (!args.with.is_empty()).then(toml::Table::new))
    else {
        // No metadata: just run it with an interpreter.
        let request = args
            .python
            .as_deref()
            .map(PythonRequest::parse)
            .unwrap_or(PythonRequest::Any);
        return Ok(python::find_or_install(&request, None, true)?.executable);
    };
    let config = script_config(script, &block, &args.with)?;
    let request = args
        .python
        .as_deref()
        .or(config.python.as_deref())
        .map(PythonRequest::parse)
        .unwrap_or(PythonRequest::Any);
    let interp = python::find_or_install(&request, config.requires_python.as_deref(), true)?;

    let key = python::hex_sha256(
        format!(
            "{}\n{}\n{:?}\n{}\n{}",
            toml::to_string(&block).unwrap_or_default(),
            meta.pylock.as_deref().unwrap_or(""),
            args.with,
            interp.executable.display(),
            env!("CARGO_PKG_VERSION")
        )
        .as_bytes(),
    );
    let dir = cache_base_dir().join("script-envs").join(&key[..24]);
    let done = dir.join(".vyp-complete");
    if done.exists() {
        if let Ok(layout) = VenvLayout::from_root(&dir) {
            return Ok(layout.python_exe);
        }
    }

    let layout = env::create(
        &dir,
        &interp,
        &VenvOptions {
            clear: true,
            ..Default::default()
        },
    )?;
    let lock = match &meta.pylock {
        Some(text) if args.with.is_empty() => toml::from_str::<LockFile>(text).map_err(|e| {
            miette::miette!(
                "Invalid `# /// pylock` block in {}: {}",
                script.display(),
                e
            )
        })?,
        _ => {
            let mut config = config.clone();
            config.target_python = Some(layout.python_exe.clone());
            let (out, _) = resolve_from_config(&config, &[], None, None)?;
            match out {
                ResolveOut::Single(result) => LockFile::from_resolution(&result),
                ResolveOut::Universal { .. } => {
                    return Err(miette::miette!("Scripts can't use [tool.vyp] environments"))
                }
            }
        }
    };
    install_lockfile(
        &lock,
        &layout,
        &InstallOptions {
            default_index: Some(config.index_url.clone()),
            startup: config.startup_files.clone(),
            quiet: true,
            lock_dir: config.project_dir.clone(),
            ..Default::default()
        },
    )?;
    crate::env::provenance::record_install(&layout, &lock)?;
    std::fs::write(&done, b"").map_err(|e| miette::miette!("{}", e))?;
    Ok(layout.python_exe)
}

/// Replace this process with `program` (POSIX), or run it and exit with its
/// status. With a `venv`, its scripts directory leads `PATH`.
fn exec(
    program: &Path,
    args: impl Iterator<Item = std::ffi::OsString>,
    venv: Option<&VenvLayout>,
) -> miette::Result<()> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    if let Some(layout) = venv {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut parts = vec![layout.scripts.clone()];
        parts.extend(std::env::split_paths(&path));
        cmd.env(
            "PATH",
            std::env::join_paths(parts).map_err(|e| miette::miette!("{}", e))?,
        );
        cmd.env("VIRTUAL_ENV", &layout.root);
        cmd.env_remove("PYTHONHOME");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(miette::miette!(
            "Failed to run {}: {}",
            program.display(),
            err
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .map_err(|e| miette::miette!("Failed to run {}: {}", program.display(), e))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Write `lock` into the script as a `# /// pylock` block at its end,
/// replacing an existing one.
pub fn write_script_lock(script: &Path, lock: &LockFile) -> miette::Result<()> {
    let source = std::fs::read_to_string(script)
        .map_err(|e| miette::miette!("Cannot read {}: {}", script.display(), e))?;
    let lock_toml = toml::to_string_pretty(lock).map_err(|e| miette::miette!("{}", e))?;
    let mut block = String::from("# /// pylock\n");
    for line in lock_toml.lines() {
        if line.is_empty() {
            block.push_str("#\n");
        } else {
            block.push_str("# ");
            block.push_str(line);
            block.push('\n');
        }
    }
    block.push_str("# ///\n");

    let mut out = String::new();
    let mut lines = source.lines().peekable();
    let mut replaced = false;
    while let Some(line) = lines.next() {
        if line.trim_end() == "# /// pylock" {
            for inner in lines.by_ref() {
                if inner.trim_end() == "# ///" {
                    break;
                }
            }
            out.push_str(&block);
            replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !replaced {
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&block);
    }
    std::fs::write(script, out)
        .map_err(|e| miette::miette!("Cannot write {}: {}", script.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "#!/usr/bin/env python\n# /// script\n# requires-python = \">=3.11\"\n# dependencies = [\n#   \"rich\",\n# ]\n#\n# [tool.vyp]\n# exclude-newer = \"2025-01-01\"\n# ///\nprint('hi')\n";

    #[test]
    fn parses_pep723_blocks() {
        let blocks = parse_blocks(SCRIPT).unwrap();
        assert_eq!(blocks.len(), 1);
        let table: toml::Table = toml::from_str(&blocks[0].1).unwrap();
        assert_eq!(table["dependencies"].as_array().unwrap().len(), 1);
        assert_eq!(table["requires-python"].as_str(), Some(">=3.11"));
    }

    #[test]
    fn rejects_malformed_blocks() {
        assert!(parse_blocks("# /// script\n# x = 1\n").is_err(), "unclosed");
        assert!(
            parse_blocks("# /// script\nx = 1\n# ///\n").is_err(),
            "non-comment line"
        );
        assert!(
            parse_blocks("# /// script\n# ///\n# /// script\n# ///\n").is_err(),
            "duplicate"
        );
        assert!(parse_blocks("# ///script\nprint()\n").unwrap().is_empty());
    }

    #[test]
    fn script_config_carries_tool_settings() {
        let table: toml::Table = toml::from_str(&parse_blocks(SCRIPT).unwrap()[0].1).unwrap();
        let config = script_config(Path::new("/tmp/s.py"), &table, &["httpx".into()]).unwrap();
        assert_eq!(config.dependencies, vec!["rich", "httpx"]);
        assert_eq!(config.requires_python.as_deref(), Some(">=3.11"));
        assert!(config.exclude_newer.default.is_some());
    }

    #[test]
    fn lock_block_round_trips_and_replaces() {
        let dir = std::env::temp_dir().join(format!("vyp-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("s.py");
        std::fs::write(&script, SCRIPT).unwrap();
        let lock = LockFile {
            lock_version: "1.0".into(),
            created_by: "vyp test".into(),
            requires_python: None,
            environments: vec![],
            extras: vec![],
            dependency_groups: vec![],
            default_groups: vec![],
            packages: vec![],
            tool: None,
        };
        write_script_lock(&script, &lock).unwrap();
        write_script_lock(&script, &lock).unwrap();
        let meta = read_metadata(&script).unwrap();
        assert!(meta.script.is_some());
        let parsed: LockFile = toml::from_str(meta.pylock.as_deref().unwrap()).unwrap();
        assert_eq!(parsed.created_by, "vyp test");
        let text = std::fs::read_to_string(&script).unwrap();
        assert_eq!(text.matches("# /// pylock").count(), 1);
        assert!(text.starts_with("#!/usr/bin/env python\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
