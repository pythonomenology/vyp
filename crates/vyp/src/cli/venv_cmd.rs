use std::path::PathBuf;

use clap::Args;

use crate::cache::venv::VenvLayout;
use crate::config::settings::VypConfig;
use crate::env::{self, provenance, VenvOptions};
use crate::python::{self, PythonRequest};

#[derive(Args)]
pub struct VenvArgs {
    /// Where to create the environment (default: .venv).
    pub path: Option<PathBuf>,

    /// Interpreter to use: a version (3.12, 3.13t), a specifier (>=3.10), or a path.
    #[arg(short, long)]
    pub python: Option<String>,

    /// Make the environment relocatable: scripts and activation find it
    /// relative to themselves, so it can be moved or copied.
    #[arg(long)]
    pub relocatable: bool,

    /// Replace an existing environment at the path.
    #[arg(long)]
    pub clear: bool,

    /// Prompt shown when activated (default: the directory name).
    #[arg(long)]
    pub prompt: Option<String>,

    /// Give the environment access to the base interpreter's site-packages.
    #[arg(long)]
    pub system_site_packages: bool,

    /// Don't download a managed Python if no installed one matches.
    #[arg(long)]
    pub no_python_downloads: bool,

    /// Check an existing environment against what it was built from instead
    /// of creating one. Exits non-zero if it has drifted.
    #[arg(long, conflicts_with_all = ["clear", "relocatable", "python"])]
    pub check: bool,

    /// Path to pyproject.toml (for requires-python).
    #[arg(long, default_value = "pyproject.toml")]
    pub project: PathBuf,
}

pub fn run(args: VenvArgs) -> miette::Result<()> {
    let config = if args.project.exists() { Some(VypConfig::from_file(&args.project)?) } else { None };
    let project_dir = config.as_ref().and_then(|c| c.project_dir.clone());
    let path = args
        .path
        .clone()
        .unwrap_or_else(|| project_dir.clone().unwrap_or_else(|| PathBuf::from(".")).join(".venv"));

    if args.check {
        let layout = VenvLayout::from_root(&std::fs::canonicalize(&path).unwrap_or(path.clone()))?;
        let drift = provenance::check(&layout);
        if drift.is_empty() {
            println!("{} matches what it was built from.", layout.root.display());
            return Ok(());
        }
        println!("{} has drifted:", layout.root.display());
        for d in &drift {
            println!("  - {}", d.describe());
        }
        return Err(miette::miette!("{} problem(s) found", drift.len()));
    }

    let requires_python = config.as_ref().and_then(|c| c.requires_python.clone());
    let request = args
        .python
        .as_deref()
        .or(config.as_ref().and_then(|c| c.python.as_deref()))
        .map(PythonRequest::parse)
        .unwrap_or(PythonRequest::Any);
    let interp = python::find_or_install(&request, requires_python.as_deref(), !args.no_python_downloads)?;

    let layout = env::create(
        &path,
        &interp,
        &VenvOptions {
            relocatable: args.relocatable,
            prompt: args.prompt,
            clear: args.clear,
            system_site_packages: args.system_site_packages,
        },
    )?;
    println!(
        "Created {}virtual environment at {} with {}",
        if args.relocatable { "relocatable " } else { "" },
        layout.root.display(),
        interp.describe()
    );
    println!("Activate with: source {}/activate", layout.scripts.display());
    Ok(())
}
