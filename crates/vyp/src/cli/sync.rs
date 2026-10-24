use std::collections::HashSet;
use std::path::PathBuf;

use clap::Args;
use vyp_api::normalize_package_name;

use crate::cache::installed::{installed_distributions, uninstall_dist};
use crate::cache::venv::VenvLayout;
use crate::config::settings::VypConfig;
use crate::lock::lockfile::LockFile;
use super::common::{install_lockfile, resolve_from_config, ResolveOut};

/// Distributions that bootstrap a venv and must never be pruned by `sync`.
const PROTECTED: &[&str] = &["pip", "setuptools", "wheel", "distribute", "vyp"];

#[derive(Args)]
pub struct SyncArgs {
    /// Path to pyproject.toml.
    #[arg(short, long, default_value = "pyproject.toml")]
    pub project: PathBuf,

    /// Sync from an existing lock file instead of resolving.
    #[arg(short, long)]
    pub lockfile: Option<PathBuf>,

    /// Target virtual environment path (auto-detected if not specified).
    #[arg(long)]
    pub venv: Option<PathBuf>,

    /// Show what would change without modifying the environment.
    #[arg(long)]
    pub dry_run: bool,

    /// PyTorch accelerator backend (auto, cpu, cu126, cu128, cu130, rocm6, xpu).
    #[arg(long)]
    pub torch_backend: Option<String>,
}

pub fn run(args: SyncArgs) -> miette::Result<()> {
    // Determine the target lock file: an explicit lock, or resolve the project.
    let (lockfile, default_index, shared_client) = if let Some(ref path) = args.lockfile {
        if !path.exists() {
            return Err(miette::miette!("Lock file not found: {}", path.display()));
        }
        (LockFile::read_from_file(path)?, None, None)
    } else {
        let config = if args.project.exists() {
            VypConfig::from_file(&args.project)?
        } else {
            return Err(miette::miette!(
                "No pyproject.toml found at {}. Use --lockfile to sync from a lock.",
                args.project.display()
            ));
        };
        let (resolve_out, client) =
            resolve_from_config(&config, &[], args.torch_backend.as_deref(), None)?;
        let lockfile = match &resolve_out {
            ResolveOut::Single(result) => LockFile::from_resolution(result),
            ResolveOut::Universal { entries, environments, errata_feeds } => LockFile::from_universal_resolution(
                entries,
                environments,
                config.requires_python.as_deref(),
                errata_feeds,
            ),
        };
        (lockfile, Some(config.index_url.clone()), Some(client))
    };

    // The set of package names the environment should contain.
    let target: HashSet<String> = lockfile
        .packages
        .iter()
        .map(|p| normalize_package_name(&p.name))
        .collect();

    // Prune distributions not in the lock (before install, so freed space can
    // be reused). Protected bootstrap packages are never removed.
    let layout = VenvLayout::discover(args.venv.as_deref())?;
    let protected: HashSet<String> = PROTECTED.iter().map(|s| s.to_string()).collect();

    let mut to_remove = Vec::new();
    for dist in installed_distributions(&layout) {
        if !target.contains(&dist.name) && !protected.contains(&dist.name) {
            to_remove.push(dist);
        }
    }

    if args.dry_run {
        println!("Would remove {} extraneous package(s):", to_remove.len());
        for d in &to_remove {
            println!("  - {} == {}", d.name, d.version);
        }
        println!("Would install/verify {} package(s) from lock.", lockfile.packages.len());
        let _ = install_lockfile(&lockfile, args.venv.as_deref(), true, default_index.as_deref(), shared_client)?;
        return Ok(());
    }

    for dist in &to_remove {
        let n = uninstall_dist(&layout, dist)
            .map_err(|e| miette::miette!("Failed to remove {}: {}", dist.name, e))?;
        println!("Removed {} == {} ({} files)", dist.name, dist.version, n);
    }

    // Install/link anything missing from the lock.
    install_lockfile(&lockfile, args.venv.as_deref(), false, default_index.as_deref(), shared_client)?;

    println!("Environment synced to lock ({} packages).", lockfile.packages.len());
    Ok(())
}
