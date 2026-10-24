use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};

use crate::config::settings::VypConfig;

#[derive(Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommands,
}

#[derive(Subcommand)]
pub enum PluginCommands {
    /// List loaded plugins
    List {
        /// Path to pyproject.toml
        #[arg(short, long, default_value = "pyproject.toml")]
        project: PathBuf,
    },
    /// Show details about a specific plugin
    Info {
        /// Plugin name
        name: String,
        /// Path to pyproject.toml
        #[arg(short, long, default_value = "pyproject.toml")]
        project: PathBuf,
    },
}

pub fn run(args: PluginArgs) -> miette::Result<()> {
    match args.command {
        PluginCommands::List { project } => run_list(&project),
        PluginCommands::Info { name, project } => run_info(&project, &name),
    }
}

fn run_list(project: &Path) -> miette::Result<()> {
    let mut loader = vyp_core::plugin::loader::PluginLoader::new();

    // Always show built-in strategies
    println!("Built-in strategies:");
    println!("  transitive-fork   (conflict strategy, priority 100)");
    println!("  override          (conflict strategy, registered when overrides configured)");
    println!("  substitution      (conflict strategy + filter, registered when substitutions configured)");
    println!("  pre-release       (resolution filter, registered when pre-releases disallowed)");

    // Load configured plugins
    if project.exists() {
        let config = VypConfig::from_file(project)?;
        config.load_plugins(&mut loader);

        let loaded = loader.loaded_plugins();
        if loaded.is_empty() {
            println!("\nNo external plugins loaded.");
        } else {
            println!("\nExternal plugins:");
            for plugin in loaded {
                println!(
                    "  {} v{} (from {})",
                    plugin.name, plugin.version, plugin.source
                );
            }
        }
    } else {
        println!("\nNo pyproject.toml found; no external plugins to load.");
    }

    Ok(())
}

fn run_info(project: &Path, name: &str) -> miette::Result<()> {
    let mut loader = vyp_core::plugin::loader::PluginLoader::new();

    if project.exists() {
        let config = VypConfig::from_file(project)?;
        config.load_plugins(&mut loader);
    }

    // Check built-in plugins
    match name {
        "transitive-fork" => {
            println!("Plugin: transitive-fork");
            println!("  Type: ConflictStrategy (built-in)");
            println!("  Priority: 100");
            println!("  Description: Detects transitive conflicts and produces Fork verdicts");
        }
        "override" => {
            println!("Plugin: override");
            println!("  Type: ConflictStrategy (built-in)");
            println!("  Priority: 50");
            println!("  Description: Rewrites version ranges based on configured override rules");
        }
        "substitution" => {
            println!("Plugin: substitution");
            println!("  Type: ConflictStrategy + ResolutionFilter (built-in)");
            println!("  Priority: 30 (strategy), 30 (filter)");
            println!("  Description: Handles package substitution/alternatives");
        }
        "pre-release" => {
            println!("Plugin: pre-release");
            println!("  Type: ResolutionFilter (built-in)");
            println!("  Priority: 50");
            println!("  Description: Excludes pre-release versions when policy disallows them");
        }
        _ => {
            // Check external plugins
            let found = loader.loaded_plugins().iter().find(|p| p.name == name);

            if let Some(plugin) = found {
                println!("Plugin: {}", plugin.name);
                println!("  Version: {}", plugin.version);
                println!("  Source: {}", plugin.source);
            } else {
                return Err(miette::miette!("Plugin '{}' not found", name));
            }
        }
    }

    Ok(())
}
