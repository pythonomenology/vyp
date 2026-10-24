use clap::Args;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use vyp_core::ResolveProgress;

use crate::config::settings::VypConfig;
use crate::lock::lockfile::LockFile;
use super::common::{resolve_from_config, ResolveOut};

#[derive(Args)]
pub struct LockArgs {
    /// Path to pyproject.toml
    #[arg(short, long, default_value = "pyproject.toml")]
    pub project: PathBuf,

    /// Output lock file path
    #[arg(short, long, default_value = "pylock.toml")]
    pub output: PathBuf,

    /// Named lock file (creates pylock.<name>.toml)
    #[arg(short, long, conflicts_with = "output")]
    pub name: Option<String>,

    /// PyTorch accelerator backend (auto, cpu, cu126, cu128, cu130, rocm6, xpu)
    #[arg(long)]
    pub torch_backend: Option<String>,
}

pub fn run(args: LockArgs) -> miette::Result<()> {
    let config = if args.project.exists() {
        VypConfig::from_file(&args.project)?
    } else {
        VypConfig::default()
    };

    let is_tty = std::io::stderr().is_terminal();
    let start = std::time::Instant::now();

    let has_printed = AtomicBool::new(false);
    let progress: Option<Box<dyn Fn(ResolveProgress) + Send>> = if is_tty {
        Some(Box::new(move |event| {
            let mut stderr = std::io::stderr().lock();
            match event {
                ResolveProgress::Selecting { package, version } => {
                    let elapsed = start.elapsed().as_secs_f64();
                    let _ = write!(
                        stderr,
                        "\x1b[2K\rLocking {package}=={version} [{elapsed:.1}s]"
                    );
                    let _ = stderr.flush();
                    has_printed.store(true, Ordering::Relaxed);
                }
                ResolveProgress::Complete { package_count } => {
                    let elapsed = start.elapsed().as_secs_f64();
                    if has_printed.load(Ordering::Relaxed) {
                        let _ = write!(stderr, "\x1b[2K\r");
                    }
                    let _ = writeln!(
                        stderr,
                        "Resolved {package_count} packages in {elapsed:.2}s"
                    );
                    let _ = stderr.flush();
                }
                _ => {}
            }
        }))
    } else {
        eprintln!("Resolving dependencies...");
        None
    };

    let (resolve_out, _client) =
        resolve_from_config(&config, &[], args.torch_backend.as_deref(), progress)?;

    let lockfile = match &resolve_out {
        ResolveOut::Single(result) => {
            if !is_tty {
                let elapsed = start.elapsed();
                eprintln!(
                    "Resolved {} packages in {:.2}s",
                    result.packages.len(),
                    elapsed.as_secs_f64()
                );
            }
            LockFile::from_resolution(result)
        }
        ResolveOut::Universal { entries, environments, errata_feeds } => {
            if !is_tty {
                let elapsed = start.elapsed();
                eprintln!(
                    "Resolved {} package entries ({} environments) in {:.2}s",
                    entries.len(),
                    environments.len(),
                    elapsed.as_secs_f64()
                );
            }
            LockFile::from_universal_resolution(
                entries,
                environments,
                config.requires_python.as_deref(),
                errata_feeds,
            )
        }
    };

    let output = if let Some(ref name) = args.name {
        PathBuf::from(LockFile::default_filename(Some(name)))
    } else {
        args.output.clone()
    };

    lockfile.write_to_file(&output)?;
    println!("Lock file written to {}", output.display());
    Ok(())
}
