pub mod add;
pub mod cache_cmd;
pub mod common;
pub mod conflict;
pub mod diff;
pub mod explain;
pub mod install;
pub mod lock;
pub mod override_cmd;
pub mod plugin_cmd;
pub mod python_cmd;
pub mod resolve;
pub mod run;
pub mod sync;
pub mod uninstall;
pub mod venv_cmd;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum Commands {
    /// Add dependency specs to pyproject.toml
    Add(add::AddArgs),
    /// Resolve dependencies and display the solution
    Resolve(resolve::ResolveArgs),
    /// Generate or update the lock file
    Lock(lock::LockArgs),
    /// Resolve and install packages into a virtual environment
    Install(install::InstallArgs),
    /// Remove installed packages from a virtual environment
    Uninstall(uninstall::UninstallArgs),
    /// Make a virtual environment exactly match the lock (install + prune)
    Sync(sync::SyncArgs),
    /// Explain why a package version was chosen
    Explain(explain::ExplainArgs),
    /// Compare two lock files
    Diff(diff::DiffArgs),
    /// Manage conflict declarations
    Conflict(conflict::ConflictArgs),
    /// Manage dependency overrides
    Override(override_cmd::OverrideArgs),
    /// Manage and inspect plugins
    Plugin(plugin_cmd::PluginArgs),
    /// Manage the vyp cache
    Cache(cache_cmd::CacheArgs),
    /// Create a virtual environment
    Venv(venv_cmd::VenvArgs),
    /// Find and install Python interpreters
    Python(python_cmd::PythonArgs),
    /// Run a PEP 723 script, or a command in the project environment
    Run(run::RunArgs),
}

pub fn run(command: Commands) -> miette::Result<()> {
    match command {
        Commands::Add(args) => add::run(args),
        Commands::Resolve(args) => resolve::run(args),
        Commands::Lock(args) => lock::run(args),
        Commands::Install(args) => install::run(args),
        Commands::Uninstall(args) => uninstall::run(args),
        Commands::Sync(args) => sync::run(args),
        Commands::Explain(args) => explain::run(args),
        Commands::Diff(args) => diff::run(args),
        Commands::Conflict(args) => conflict::run(args),
        Commands::Override(args) => override_cmd::run(args),
        Commands::Plugin(args) => plugin_cmd::run(args),
        Commands::Cache(args) => cache_cmd::run(args),
        Commands::Venv(args) => venv_cmd::run(args),
        Commands::Python(args) => python_cmd::run(args),
        Commands::Run(args) => run::run(args),
    }
}
