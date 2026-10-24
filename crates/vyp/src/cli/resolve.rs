use clap::Args;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use vyp_api::Requirement;
use vyp_core::ResolveProgress;

use crate::config::settings::VypConfig;
use super::common::{resolve_from_config, ResolveOut};

#[derive(Args)]
pub struct ResolveArgs {
    /// Path to pyproject.toml (defaults to ./pyproject.toml)
    #[arg(short, long, default_value = "pyproject.toml")]
    pub project: PathBuf,

    /// Additional requirements to resolve (e.g. "numpy>=1.20")
    #[arg(short, long)]
    pub requirement: Vec<String>,

    /// PyTorch accelerator backend (auto, cpu, cu126, cu128, cu130, rocm6, xpu)
    #[arg(long)]
    pub torch_backend: Option<String>,
}

pub fn run(args: ResolveArgs) -> miette::Result<()> {
    let config = if args.project.exists() {
        VypConfig::from_file(&args.project)?
    } else {
        VypConfig::default()
    };

    let extra_reqs: Vec<Requirement> = args
        .requirement
        .iter()
        .map(|s| {
            s.parse()
                .map_err(|e| miette::miette!("Invalid requirement '{}': {}", s, e))
        })
        .collect::<miette::Result<Vec<_>>>()?;

    let dep_count = config.dependencies.len() + extra_reqs.len();
    let is_tty = std::io::stderr().is_terminal();
    let start = std::time::Instant::now();

    let has_printed = AtomicBool::new(false);
    let progress: Option<Box<dyn Fn(ResolveProgress) + Send>> = if is_tty {
        Some(Box::new(move |event| {
            let mut stderr = std::io::stderr().lock();
            match event {
                ResolveProgress::Selecting { package, version } => {
                    let elapsed = start.elapsed().as_secs_f64();
                    // \x1b[2K clears the entire current line, \r returns to column 0
                    let _ = write!(
                        stderr,
                        "\x1b[2K\rResolving ({dep_count}) {package}=={version} [{elapsed:.1}s]"
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
        eprintln!("Resolving {} dependencies...", dep_count);
        None
    };

    let (resolve_out, _client) = resolve_from_config(&config, &extra_reqs, args.torch_backend.as_deref(), progress)?;

    match &resolve_out {
        ResolveOut::Single(result) => {
            if !is_tty {
                let elapsed = start.elapsed();
                eprintln!(
                    "Resolved {} packages in {:.2}s",
                    result.packages.len(),
                    elapsed.as_secs_f64()
                );
            }
            println!("\nResolution successful!\n");
            let mut sorted: Vec<_> = result.packages.iter().collect();
            sorted.sort_by_key(|(name, _)| (*name).clone());
            for (name, version) in sorted {
                println!("  {} == {}", name, version);
            }
            if !result.inherited_conflicts.is_empty() {
                println!("\nInherited conflicts:");
                for (pkg, conflicts) in &result.inherited_conflicts {
                    for decl in &conflicts.declarations {
                        println!("  {}: {}", pkg, decl);
                    }
                }
            }
        }
        ResolveOut::Universal { entries, environments, .. } => {
            if !is_tty {
                let elapsed = start.elapsed();
                eprintln!(
                    "Resolved {} package entries ({} environments) in {:.2}s",
                    entries.len(),
                    environments.len(),
                    elapsed.as_secs_f64()
                );
            }
            println!("\nResolution successful! (universal)\n");
            let mut sorted: Vec<_> = entries.iter().collect();
            sorted.sort_by(|a, b| a.name.cmp(&b.name).then(a.marker.cmp(&b.marker)));
            for e in sorted {
                let marker_str = e.marker.as_deref().unwrap_or("(all)");
                println!("  {} == {}  [{}]", e.name, e.version, marker_str);
            }
        }
    }

    if let ResolveOut::Single(result) = &resolve_out {
        if let Some(ref timing) = result.timing {
            use std::io::Write;
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "\n--- vyp profile ---");
            let _ = writeln!(stderr, "Resolve:        {:>8.1}ms  ({} iterations)", timing.total_ms, timing.iterations);
            let vdh = timing.provider_counters.get("version_disk_hits").copied().unwrap_or(0);
            let v304 = timing.provider_counters.get("version_304s").copied().unwrap_or(0);
            let vnet = timing.provider_counters.get("version_fresh_fetches").copied().unwrap_or(0);
            let _ = writeln!(stderr, "  version wait: {:>8.1}ms  ({} fetches: {} disk, {} 304, {} network)", timing.version_wait_ms, timing.version_fetches, vdh, v304, vnet);
            let mdh = timing.provider_counters.get("metadata_disk_hits").copied().unwrap_or(0);
            let mnet = timing.provider_counters.get("metadata_network_fetches").copied().unwrap_or(0);
            let _ = writeln!(stderr, "  meta wait:    {:>8.1}ms  ({} fetches: {} disk, {} network)", timing.metadata_wait_ms, timing.metadata_fetches, mdh, mnet);
            let _ = writeln!(stderr, "  solver:       {:>8.1}ms", timing.solver_ms);
            let _ = writeln!(stderr, "--- end profile ---");
        }
    }

    Ok(())
}
