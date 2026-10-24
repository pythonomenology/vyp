use std::path::PathBuf;

use clap::Args;

use crate::cache::installed::{find_installed, uninstall_dist};
use crate::cache::venv::VenvLayout;

#[derive(Args)]
pub struct UninstallArgs {
    /// Packages to uninstall.
    pub packages: Vec<String>,

    /// Target virtual environment path (auto-detected if not specified).
    #[arg(long)]
    pub venv: Option<PathBuf>,

    /// Show what would be removed without removing anything.
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: UninstallArgs) -> miette::Result<()> {
    if args.packages.is_empty() {
        return Err(miette::miette!("No packages specified to uninstall."));
    }

    let layout = VenvLayout::discover(args.venv.as_deref())?;

    let mut not_found = Vec::new();
    let mut removed_total = 0usize;

    for name in &args.packages {
        match find_installed(&layout, name) {
            Some(dist) => {
                if args.dry_run {
                    println!(
                        "Would remove {} == {} ({})",
                        dist.name,
                        dist.version,
                        dist.dist_info.display()
                    );
                    continue;
                }
                let removed = uninstall_dist(&layout, &dist)
                    .map_err(|e| miette::miette!("Failed to uninstall {}: {}", name, e))?;
                removed_total += removed;
                println!(
                    "Uninstalled {} == {} ({} files)",
                    dist.name, dist.version, removed
                );
            }
            None => not_found.push(name.clone()),
        }
    }

    if !not_found.is_empty() {
        for n in &not_found {
            println!("Skipping {}: not installed", n);
        }
    }

    if !args.dry_run {
        println!("Removed {} file(s) total.", removed_total);
    }

    Ok(())
}
