use crate::cache::wheel_cache::cache_base_dir;
use clap::{Args, Subcommand};

#[derive(Args)]
pub struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Subcommand)]
enum CacheCommand {
    /// Print the cache directory path
    Dir,
    /// Show cache size breakdown
    Info,
    /// Remove all cached data
    Clean {
        /// Only remove entries for specific packages
        #[arg(long, num_args = 1..)]
        packages: Option<Vec<String>>,
    },
    /// Remove stale and orphaned entries
    Prune,
}

pub fn run(args: CacheArgs) -> miette::Result<()> {
    match args.command {
        CacheCommand::Dir => {
            println!("{}", cache_base_dir().display());
            Ok(())
        }
        CacheCommand::Info => run_info(),
        CacheCommand::Clean { packages } => run_clean(packages),
        CacheCommand::Prune => run_prune(),
    }
}

fn run_info() -> miette::Result<()> {
    let base = cache_base_dir();
    let metadata_dir = base.join("cache").join("metadata");
    let archive_dir = base.join("archive");
    let tmp_dir = base.join("tmp");

    let metadata_size = dir_size(&metadata_dir);
    let archive_size = dir_size(&archive_dir);
    let tmp_size = dir_size(&tmp_dir);
    let total = metadata_size + archive_size + tmp_size;

    println!("Cache directory: {}", base.display());
    println!();
    println!("  Metadata cache:  {}", format_size(metadata_size));
    println!("  Archive cache:   {}", format_size(archive_size));
    println!("  Temp files:      {}", format_size(tmp_size));
    println!("  ─────────────────────────");
    println!("  Total:           {}", format_size(total));

    let metadata_count = count_files(&metadata_dir);
    let archive_count = count_dirs(&archive_dir);
    println!();
    println!("  Metadata entries: {}", metadata_count);
    println!("  Cached archives:  {}", archive_count);

    Ok(())
}

fn run_clean(packages: Option<Vec<String>>) -> miette::Result<()> {
    let base = cache_base_dir();

    if let Some(ref names) = packages {
        let archive_dir = base.join("archive");
        let metadata_dir = base.join("cache").join("metadata");
        let mut removed = 0;

        for name in names {
            let normalized = name.to_lowercase().replace(['-', '.'], "_");

            if archive_dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&archive_dir) {
                    for entry in entries.flatten() {
                        let fname = entry.file_name().to_string_lossy().to_string();
                        if fname.starts_with(&format!("{}-", normalized))
                            || fname.starts_with(&format!("{}_", normalized))
                        {
                            let _ = std::fs::remove_dir_all(entry.path());
                            removed += 1;
                        }
                    }
                }
            }

            if metadata_dir.exists() {
                let versions_file = metadata_dir.join(format!("versions-{}.bin", normalized));
                if versions_file.exists() {
                    let _ = std::fs::remove_file(&versions_file);
                    removed += 1;
                }
            }
        }
        println!(
            "Removed {} entries for packages: {}",
            removed,
            names.join(", ")
        );
    } else {
        let before = dir_size(&base);
        let _ = std::fs::remove_dir_all(&base);
        println!("Cleared cache ({} freed)", format_size(before));
    }

    Ok(())
}

fn run_prune() -> miette::Result<()> {
    let base = cache_base_dir();
    let mut removed = 0u64;

    let tmp_dir = base.join("tmp");
    if tmp_dir.exists() {
        removed += dir_size(&tmp_dir);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    let metadata_dir = base.join("cache").join("metadata");
    if metadata_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&metadata_dir) {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if !fname.starts_with("versions-") {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    let modified = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if now.saturating_sub(modified) > 7 * 24 * 3600 {
                        removed += meta.len();
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    let archive_dir = base.join("archive");
    if archive_dir.exists() {
        let _ = reconcile_orphaned_tmp_dirs(&archive_dir);
    }

    println!("Pruned {} of stale data", format_size(removed));
    Ok(())
}

fn reconcile_orphaned_tmp_dirs(archive_dir: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(archive_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && name.ends_with(".tmp") {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(())
}

fn dir_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn count_files(path: &std::path::Path) -> usize {
    if !path.exists() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|entries| entries.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

fn count_dirs(path: &std::path::Path) -> usize {
    if !path.exists() {
        return 0;
    }
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}
