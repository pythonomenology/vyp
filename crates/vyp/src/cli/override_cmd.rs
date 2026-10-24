use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct OverrideArgs {
    #[command(subcommand)]
    pub command: OverrideCommands,
}

#[derive(Subcommand)]
pub enum OverrideCommands {
    /// Add a dependency override for a package
    Add {
        /// Package name
        package: String,
        /// Version constraint (e.g. ">=1.26,<2")
        constraint: String,
        /// Make this override transitive
        #[arg(long)]
        transitive: bool,
        /// Reason for the override
        #[arg(long)]
        reason: Option<String>,
        /// Path to pyproject.toml
        #[arg(short, long, default_value = "pyproject.toml")]
        project: PathBuf,
    },
    /// Remove a dependency override
    Remove {
        /// Package name
        package: String,
        /// Path to pyproject.toml
        #[arg(short, long, default_value = "pyproject.toml")]
        project: PathBuf,
    },
    /// List all overrides (transitive overrides are marked)
    List {
        /// Path to pyproject.toml
        #[arg(short, long, default_value = "pyproject.toml")]
        project: PathBuf,
    },
    /// Export transitive overrides to a TOML file for library consumers
    Export {
        /// Path to pyproject.toml
        #[arg(short, long, default_value = "pyproject.toml")]
        project: PathBuf,
        /// Output file path
        #[arg(short, long, default_value = "vyp-overrides.toml")]
        output: PathBuf,
    },
}

pub fn run(args: OverrideArgs) -> miette::Result<()> {
    match args.command {
        OverrideCommands::Add {
            package,
            constraint,
            transitive,
            reason,
            project,
        } => run_add(
            &project,
            &package,
            &constraint,
            transitive,
            reason.as_deref(),
        ),
        OverrideCommands::Remove { package, project } => run_remove(&project, &package),
        OverrideCommands::List { project } => run_list(&project),
        OverrideCommands::Export { project, output } => run_export(&project, &output),
    }
}

fn run_add(
    project: &Path,
    package: &str,
    constraint: &str,
    transitive: bool,
    reason: Option<&str>,
) -> miette::Result<()> {
    let content = std::fs::read_to_string(project)
        .map_err(|e| miette::miette!("Failed to read {}: {}", project.display(), e))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| miette::miette!("Failed to parse pyproject.toml: {}", e))?;

    doc.entry("tool")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    doc["tool"]
        .as_table_mut()
        .ok_or_else(|| miette::miette!("[tool] exists but is not a table in pyproject.toml"))?
        .entry("vyp")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));

    let vyp = doc["tool"]["vyp"]
        .as_table_mut()
        .ok_or_else(|| miette::miette!("[tool.vyp] exists but is not a table in pyproject.toml"))?;
    if vyp.get("overrides").is_none() {
        vyp.insert(
            "overrides",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }

    let overrides = vyp["overrides"]
        .as_array_of_tables_mut()
        .ok_or_else(|| miette::miette!("[[tool.vyp.overrides]] is not an array of tables"))?;

    let mut entry = toml_edit::Table::new();
    entry.insert("package", toml_edit::value(package));
    entry.insert("constraint", toml_edit::value(constraint));
    if transitive {
        entry.insert("transitive", toml_edit::value(true));
    }
    if let Some(reason) = reason {
        entry.insert("reason", toml_edit::value(reason));
    }

    overrides.push(entry);

    std::fs::write(project, doc.to_string())
        .map_err(|e| miette::miette!("Failed to write {}: {}", project.display(), e))?;

    println!(
        "Added override: {} = \"{}\"{}",
        package,
        constraint,
        if transitive { " (transitive)" } else { "" }
    );
    Ok(())
}

fn run_remove(project: &Path, package: &str) -> miette::Result<()> {
    let content = std::fs::read_to_string(project)
        .map_err(|e| miette::miette!("Failed to read {}: {}", project.display(), e))?;

    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| miette::miette!("Failed to parse pyproject.toml: {}", e))?;

    let overrides = doc
        .get_mut("tool")
        .and_then(|t| t.get_mut("vyp"))
        .and_then(|c| c.get_mut("overrides"))
        .and_then(|c| c.as_array_of_tables_mut());

    if let Some(overrides) = overrides {
        let mut idx_to_remove = None;
        for (i, table) in overrides.iter().enumerate() {
            if table
                .get("package")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n == package)
            {
                idx_to_remove = Some(i);
                break;
            }
        }

        if let Some(idx) = idx_to_remove {
            overrides.remove(idx);
            std::fs::write(project, doc.to_string())
                .map_err(|e| miette::miette!("Failed to write {}: {}", project.display(), e))?;
            println!("Removed override for '{}'", package);
        } else {
            println!("Override for '{}' not found", package);
        }
    } else {
        println!("No overrides defined in pyproject.toml");
    }

    Ok(())
}

fn run_list(project: &Path) -> miette::Result<()> {
    let config = crate::config::settings::VypConfig::from_file(project)?;

    if config.overrides.is_empty() {
        println!("No overrides configured.");
        return Ok(());
    }

    println!("Dependency overrides:");
    for dep_override in &config.overrides {
        print!(
            "  {} = \"{}\"",
            dep_override.package, dep_override.constraint
        );
        if dep_override.transitive {
            print!(" [transitive]");
        }
        if let Some(ref reason) = dep_override.reason {
            print!(" — {}", reason);
        }
        println!();
    }
    Ok(())
}

fn run_export(project: &Path, output: &Path) -> miette::Result<()> {
    use crate::config::settings::VypConfig;
    use crate::lock::conflict_overrides::OverridesExportFile;

    let config = if project.exists() {
        VypConfig::from_file(project)?
    } else {
        return Err(miette::miette!(
            "pyproject.toml not found at {}",
            project.display()
        ));
    };

    let transitive_overrides: Vec<_> = config
        .overrides
        .iter()
        .filter(|o| o.transitive)
        .cloned()
        .collect();

    if transitive_overrides.is_empty() {
        println!("No transitive overrides to export.");
        return Ok(());
    }

    let overrides_file = OverridesExportFile::from_overrides(
        "project",
        env!("CARGO_PKG_VERSION"),
        &transitive_overrides,
    );

    overrides_file.write_to_file(output)?;
    println!(
        "Exported {} transitive override(s) to {}",
        transitive_overrides.len(),
        output.display()
    );

    Ok(())
}
