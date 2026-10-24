use clap::Args;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use vyp_api::Requirement;
use vyp_core::{ResolveProgress, ResolveTiming};

use super::common::{
    install_lockfile, resolve_from_config, target_venv, InstallOptions, InstallTiming, ResolveOut,
};
use crate::config::settings::VypConfig;
use crate::lock::lockfile::LockFile;

#[derive(Args)]
pub struct InstallArgs {
    /// Packages to install (e.g. "numpy" "pandas>=2.0" "requests[security]>=2.28")
    /// If omitted, installs from pyproject.toml or lockfile.
    pub packages: Vec<String>,

    /// Path to pyproject.toml
    #[arg(short, long, default_value = "pyproject.toml")]
    pub project: PathBuf,

    /// Install from an existing lock file instead of resolving
    #[arg(short, long)]
    pub lockfile: Option<PathBuf>,

    /// Target virtual environment path (auto-detected if not specified)
    #[arg(long)]
    pub venv: Option<PathBuf>,

    /// Only print what would be installed, without installing
    #[arg(long)]
    pub dry_run: bool,

    /// PyTorch accelerator backend (auto, cpu, cu126, cu128, cu130, rocm6, xpu)
    #[arg(long)]
    pub torch_backend: Option<String>,

    /// Ignore distributions uploaded after this date, timestamp or duration
    /// ago (e.g. 2026-01-31, P7D, "7 days"). Overrides `exclude-newer`.
    #[arg(long, env = "VYP_EXCLUDE_NEWER")]
    pub exclude_newer: Option<String>,
}

pub fn run(args: InstallArgs) -> miette::Result<()> {
    let profiling = std::env::var("VYP_PROFILE").is_ok_and(|v| v == "1");

    if let Some(ref lockfile_path) = args.lockfile {
        if !lockfile_path.exists() {
            return Err(miette::miette!(
                "Lock file not found: {}. Run `vyp lock` first.",
                lockfile_path.display()
            ));
        }
        let lockfile = LockFile::read_from_file(lockfile_path)?;
        let config = if args.project.exists() {
            VypConfig::from_file(&args.project)?
        } else {
            VypConfig::default()
        };
        let layout = target_venv(&config, args.venv.as_deref())?;
        let install_timing = install_lockfile(
            &lockfile,
            &layout,
            &InstallOptions {
                dry_run: args.dry_run,
                startup: config.startup_files.clone(),
                lock_dir: lockfile_path.parent().map(|p| p.to_path_buf()),
                ..Default::default()
            },
        )?;
        if !args.dry_run {
            crate::env::provenance::record_install(&layout, &lockfile)?;
        }
        if profiling {
            print_profile(None, install_timing.as_ref());
        }
        return Ok(());
    }

    let mut config = if args.project.exists() {
        VypConfig::from_file(&args.project)?
    } else if args.packages.is_empty() {
        return Err(miette::miette!(
            "No packages specified and no pyproject.toml found at {}.",
            args.project.display()
        ));
    } else {
        VypConfig::default()
    };
    if let Some(cutoff) = &args.exclude_newer {
        config.set_exclude_newer(cutoff)?;
    }

    let extra_reqs: Vec<Requirement> = args
        .packages
        .iter()
        .map(|s| {
            s.parse::<Requirement>()
                .map_err(|e| miette::miette!("Invalid requirement '{}': {}", s, e))
        })
        .collect::<miette::Result<Vec<_>>>()?;

    let layout = target_venv(&config, args.venv.as_deref())?;
    config.target_python = Some(layout.python_exe.clone());

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
                        "\x1b[2K\rResolving {package}=={version} [{elapsed:.1}s]"
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
        None
    };

    let (resolve_out, shared_client) = resolve_from_config(
        &config,
        &extra_reqs,
        args.torch_backend.as_deref(),
        progress,
    )?;

    let (lockfile, resolve_timing) = match &resolve_out {
        ResolveOut::Single(result) => (LockFile::from_resolution(result), result.timing.clone()),
        ResolveOut::Universal {
            entries,
            environments,
            errata_feeds,
            exclude_newer,
        } => (
            LockFile::from_universal_resolution(
                entries,
                environments,
                config.requires_python.as_deref(),
                errata_feeds,
                *exclude_newer,
            ),
            None,
        ),
    };
    let install_timing = install_lockfile(
        &lockfile,
        &layout,
        &InstallOptions {
            dry_run: args.dry_run,
            default_index: Some(config.index_url.clone()),
            client: Some(shared_client),
            startup: config.startup_files.clone(),
            lock_dir: config.project_dir.clone(),
            ..Default::default()
        },
    )?;
    if !args.dry_run {
        crate::env::provenance::record_install(&layout, &lockfile)?;
    }

    if profiling {
        print_profile(resolve_timing.as_ref(), install_timing.as_ref());
    }

    Ok(())
}

fn print_profile(resolve: Option<&ResolveTiming>, install: Option<&InstallTiming>) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "\n--- vyp profile ---");

    let mut grand_total_ms = 0.0f64;

    if let Some(r) = resolve {
        grand_total_ms += r.total_ms;
        let _ = writeln!(
            stderr,
            "Resolve:        {:>8.1}ms  ({} iterations)",
            r.total_ms, r.iterations
        );

        let vdh = r
            .provider_counters
            .get("version_disk_hits")
            .copied()
            .unwrap_or(0);
        let v304 = r
            .provider_counters
            .get("version_304s")
            .copied()
            .unwrap_or(0);
        let vnet = r
            .provider_counters
            .get("version_fresh_fetches")
            .copied()
            .unwrap_or(0);
        let _ = writeln!(
            stderr,
            "  version wait: {:>8.1}ms  ({} fetches: {} disk, {} 304, {} network)",
            r.version_wait_ms, r.version_fetches, vdh, v304, vnet,
        );

        let mdh = r
            .provider_counters
            .get("metadata_disk_hits")
            .copied()
            .unwrap_or(0);
        let mnet = r
            .provider_counters
            .get("metadata_network_fetches")
            .copied()
            .unwrap_or(0);
        let _ = writeln!(
            stderr,
            "  meta wait:    {:>8.1}ms  ({} fetches: {} disk, {} network)",
            r.metadata_wait_ms, r.metadata_fetches, mdh, mnet,
        );

        let _ = writeln!(stderr, "  solver:       {:>8.1}ms", r.solver_ms);
        let _ = writeln!(stderr, "  wheel URLs:   {:>8.1}ms", r.wheel_url_ms);
    }

    if let Some(i) = install {
        grand_total_ms += i.total_ms;
        let _ = writeln!(stderr, "Install:        {:>8.1}ms  (wall)", i.total_ms);
        let _ = writeln!(stderr, "  marker_detect:   {:>8.1}ms", i.marker_detect_ms);
        let _ = writeln!(stderr, "  site_packages:   {:>8.1}ms", i.site_packages_ms);
        let _ = writeln!(
            stderr,
            "  cache_check:     {:>8.1}ms  ({} cached, {} to download)",
            i.cache_check_ms, i.cached_count, i.download_count
        );
        let _ = writeln!(stderr, "  runtime_client:  {:>8.1}ms", i.runtime_client_ms);
        let _ = writeln!(
            stderr,
            "  pipeline_wall:   {:>8.1}ms  (block_on total)",
            i.pipeline_wall_ms
        );
        let _ = writeln!(
            stderr,
            "  cached_link_wall:{:>8.1}ms  (cached pkgs link)",
            i.cached_link_wall_ms
        );
        let _ = writeln!(
            stderr,
            "  download (sum):  {:>8.1}ms  ({} wheels)",
            i.download_ms, i.download_count
        );
        let _ = writeln!(
            stderr,
            "  extract (sum):   {:>8.1}ms  ({} wheels)",
            i.extract_ms, i.download_count
        );
        let _ = writeln!(
            stderr,
            "  link (sum):      {:>8.1}ms  ({} packages)",
            i.link_ms, i.link_count
        );
        let _ = writeln!(stderr, "  eviction:        {:>8.1}ms", i.eviction_ms);
    }

    let _ = writeln!(stderr, "Total:          {:>8.1}ms", grand_total_ms);
    let _ = writeln!(stderr, "--- end profile ---");
}
