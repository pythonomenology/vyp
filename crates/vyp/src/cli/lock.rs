use clap::Args;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use vyp_core::ResolveProgress;

use super::common::{resolve_from_config, ResolveOut};
use crate::config::settings::VypConfig;
use crate::lock::lockfile::LockFile;

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

    /// Ignore distributions uploaded after this date, timestamp or duration
    /// ago (e.g. 2026-01-31, P7D, "7 days"). Overrides `exclude-newer`.
    #[arg(long, env = "VYP_EXCLUDE_NEWER")]
    pub exclude_newer: Option<String>,

    /// Lock a PEP 723 script: resolve its inline metadata and write the
    /// result into it as a `# /// pylock` block.
    #[arg(long, conflicts_with_all = ["output", "name"])]
    pub script: Option<PathBuf>,
}

pub fn run(args: LockArgs) -> miette::Result<()> {
    if let Some(script) = &args.script {
        return lock_script(script, args.exclude_newer.as_deref());
    }
    let mut config = if args.project.exists() {
        VypConfig::from_file(&args.project)?
    } else {
        VypConfig::default()
    };
    if let Some(cutoff) = &args.exclude_newer {
        config.set_exclude_newer(cutoff)?;
    }

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
                    let _ = writeln!(stderr, "Resolved {package_count} packages in {elapsed:.2}s");
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
        ResolveOut::Universal {
            entries,
            environments,
            errata_feeds,
            exclude_newer,
        } => {
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
                *exclude_newer,
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

fn lock_script(script: &std::path::Path, exclude_newer: Option<&str>) -> miette::Result<()> {
    let meta = super::run::read_metadata(script)?;
    let block = meta.script.ok_or_else(|| {
        miette::miette!("{} has no `# /// script` metadata block", script.display())
    })?;
    let mut config = super::run::script_config(script, &block, &[])?;
    if let Some(cutoff) = exclude_newer {
        config.set_exclude_newer(cutoff)?;
    }
    let request = config
        .python
        .as_deref()
        .map(crate::python::PythonRequest::parse)
        .unwrap_or(crate::python::PythonRequest::Any);
    let interp = crate::python::find_or_install(&request, config.requires_python.as_deref(), true)?;
    config.target_python = Some(interp.executable);
    let (out, _) = resolve_from_config(&config, &[], None, None)?;
    let lockfile = match out {
        ResolveOut::Single(result) => LockFile::from_resolution(&result),
        ResolveOut::Universal { .. } => {
            return Err(miette::miette!("Scripts can't use [tool.vyp] environments"))
        }
    };
    super::run::write_script_lock(script, &lockfile)?;
    println!(
        "Locked {} packages into {}",
        lockfile.packages.len(),
        script.display()
    );
    Ok(())
}
