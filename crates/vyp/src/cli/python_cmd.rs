use clap::{Args, Subcommand};

use crate::python::{self, download, PythonRequest};

#[derive(Args)]
pub struct PythonArgs {
    #[command(subcommand)]
    pub command: PythonCommand,
}

#[derive(Subcommand)]
pub enum PythonCommand {
    /// Download and install a managed Python (default: the newest).
    Install {
        /// Version to install, e.g. 3.12 or 3.12.7.
        version: Option<String>,
    },
    /// List the interpreters vyp can find.
    List,
    /// Print the interpreter vyp would use for a request.
    Find {
        /// A version (3.12), specifier (>=3.10), or path.
        request: Option<String>,
    },
    /// Remove managed installs matching a version.
    Uninstall { version: String },
    /// Print the directory managed Pythons are installed into.
    Dir,
}

pub fn run(args: PythonArgs) -> miette::Result<()> {
    match args.command {
        PythonCommand::Install { version } => {
            let request = version
                .as_deref()
                .map(PythonRequest::parse)
                .unwrap_or(PythonRequest::Any);
            let candidate = download::find_candidate(&request, None)?;
            let interp = download::install_candidate(&candidate)?;
            println!("{}", interp.describe());
        }
        PythonCommand::List => {
            for interp in python::find_all() {
                println!(
                    "{:<12} {:<9} {}{}",
                    interp.version,
                    interp.implementation,
                    interp.executable.display(),
                    if interp.managed { "  (managed)" } else { "" }
                );
            }
        }
        PythonCommand::Find { request } => {
            let request = request
                .as_deref()
                .map(PythonRequest::parse)
                .unwrap_or(PythonRequest::Any);
            let interp = python::find(&request, None).ok_or_else(|| {
                miette::miette!("No Python matching {}", python::describe_request(&request))
            })?;
            println!("{}", interp.executable.display());
        }
        PythonCommand::Uninstall { version } => {
            let removed = download::uninstall(&version)?;
            if removed.is_empty() {
                return Err(miette::miette!("No managed Python matching {}", version));
            }
            for dir in removed {
                println!("Removed {}", dir.display());
            }
        }
        PythonCommand::Dir => println!("{}", python::managed_dir().display()),
    }
    Ok(())
}
