use clap::Args;
use std::path::PathBuf;
use vyp_api::types::package::normalize_package_name;
use vyp_api::Requirement;

#[derive(Args)]
pub struct AddArgs {
    /// Packages to add (e.g. "numpy" "pandas>=2.0" "requests[security]>=2.28")
    #[arg(required = true)]
    pub packages: Vec<String>,

    /// Path to pyproject.toml
    #[arg(short, long, default_value = "pyproject.toml")]
    pub project: PathBuf,

    /// Add to an optional dependency group (PEP 621)
    #[arg(long)]
    pub optional: Option<String>,

    /// Add to a dependency group (PEP 735)
    #[arg(long)]
    pub group: Option<String>,
}

pub fn run(args: AddArgs) -> miette::Result<()> {
    if !args.project.exists() {
        return Err(miette::miette!(
            "pyproject.toml not found at {}",
            args.project.display()
        ));
    }

    let parsed_reqs: Vec<Requirement> = args
        .packages
        .iter()
        .map(|s| {
            s.parse::<Requirement>()
                .map_err(|e| miette::miette!("Invalid requirement '{}': {}", s, e))
        })
        .collect::<miette::Result<Vec<_>>>()?;

    let original_content = std::fs::read_to_string(&args.project)
        .map_err(|e| miette::miette!("Failed to read {}: {}", args.project.display(), e))?;

    let mut doc: toml_edit::DocumentMut = original_content
        .parse()
        .map_err(|e| miette::miette!("Failed to parse {}: {}", args.project.display(), e))?;

    for (raw_spec, parsed) in args.packages.iter().zip(&parsed_reqs) {
        let normalized_name = normalize_package_name(parsed.package.name());
        let target_array =
            get_or_create_target_array(&mut doc, args.optional.as_deref(), args.group.as_deref())?;

        let existing_idx = target_array.iter().position(|item| {
            item.as_str()
                .map(|s| {
                    let existing_name = s
                        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                        .next()
                        .unwrap_or("");
                    normalize_package_name(existing_name) == normalized_name
                })
                .unwrap_or(false)
        });

        if let Some(idx) = existing_idx {
            target_array.replace(idx, raw_spec.as_str());
            println!("  Updated {} -> {}", normalized_name, raw_spec);
        } else {
            target_array.push(raw_spec.as_str());
            println!("  Added {}", raw_spec);
        }
    }

    std::fs::write(&args.project, doc.to_string())
        .map_err(|e| miette::miette!("Failed to write {}: {}", args.project.display(), e))?;

    println!("pyproject.toml updated. Run `vyp install` to install.");
    Ok(())
}

fn get_or_create_target_array<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    optional: Option<&str>,
    group: Option<&str>,
) -> miette::Result<&'a mut toml_edit::Array> {
    if let Some(group_name) = group {
        let dg = doc
            .entry("dependency-groups")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let dg_table = dg
            .as_table_mut()
            .ok_or_else(|| miette::miette!("[dependency-groups] is not a table"))?;
        let arr_item = dg_table.entry(group_name).or_insert_with(|| {
            toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new()))
        });
        arr_item
            .as_array_mut()
            .ok_or_else(|| miette::miette!("[dependency-groups.{}] is not an array", group_name))
    } else if let Some(opt_name) = optional {
        let project = doc
            .entry("project")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let project_table = project
            .as_table_mut()
            .ok_or_else(|| miette::miette!("[project] is not a table"))?;
        let opt_deps = project_table
            .entry("optional-dependencies")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let opt_table = opt_deps
            .as_table_mut()
            .ok_or_else(|| miette::miette!("[project.optional-dependencies] is not a table"))?;
        let arr_item = opt_table.entry(opt_name).or_insert_with(|| {
            toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new()))
        });
        arr_item.as_array_mut().ok_or_else(|| {
            miette::miette!(
                "[project.optional-dependencies.{}] is not an array",
                opt_name
            )
        })
    } else {
        let project = doc
            .entry("project")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let project_table = project
            .as_table_mut()
            .ok_or_else(|| miette::miette!("[project] is not a table"))?;
        let arr_item = project_table.entry("dependencies").or_insert_with(|| {
            toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new()))
        });
        arr_item
            .as_array_mut()
            .ok_or_else(|| miette::miette!("[project].dependencies is not an array"))
    }
}
