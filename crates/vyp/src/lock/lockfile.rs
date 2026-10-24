use vyp_api::{SelectionReason, VariantDescriptor};
use vyp_core::{ErrataFeedDigest, ResolutionResult};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use super::universal::UniversalPackageEntry;

/// PEP 751 `pylock.toml` lock file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyLockFile {
    #[serde(rename = "lock-version")]
    pub lock_version: String,
    #[serde(rename = "created-by")]
    pub created_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "requires-python")]
    pub requires_python: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extras: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "dependency-groups")]
    pub dependency_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "default-groups")]
    pub default_groups: Vec<String>,
    pub packages: Vec<PyLockPackage>,
    /// Tool-specific, lock-level metadata (`[tool.vyp]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PyLockTool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PyLockTool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vyp: Option<VypLockTool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VypLockTool {
    /// Errata feeds consulted, with the digest of the content used.
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "errata-feeds")]
    pub errata_feeds: Vec<LockedErrataFeed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedErrataFeed {
    pub source: String,
    pub sha256: String,
}

impl PyLockTool {
    fn from_feeds(feeds: &[ErrataFeedDigest]) -> Option<Self> {
        if feeds.is_empty() {
            return None;
        }
        Some(Self {
            vyp: Some(VypLockTool {
                errata_feeds: feeds
                    .iter()
                    .map(|f| LockedErrataFeed { source: f.source.clone(), sha256: f.sha256.clone() })
                    .collect(),
            }),
        })
    }
}

/// A single package entry in `pylock.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyLockPackage {
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "requires-python")]
    pub requires_python: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wheels: Vec<PyLockWheel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdist: Option<PyLockSdist>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// PEP 825 variant descriptor for this package (if a variant wheel was selected).
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "variant")]
    pub variant: Option<VariantDescriptor>,
    /// Tool-specific metadata. PEP 751 allows `[tool]` at the package level
    /// and states it MUST NOT affect installation behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<PyLockPackageTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyLockWheel {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashes: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyLockSdist {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashes: Option<BTreeMap<String, String>>,
}

/// Vyp-specific provenance stored in `[packages.tool.vyp]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PyLockPackageTool {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vyp: Option<VypProvenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VypProvenance {
    pub selected_by: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_with: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_path: Option<String>,
    /// Overrides that replaced every requirement on this package.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<LockedOverride>,
    /// Errata that excluded releases of this package or changed the
    /// selected release's requirements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errata: Vec<LockedErratum>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedOverride {
    pub constraint: String,
    /// `pyproject.toml`, or the release that shipped the override.
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedErratum {
    /// Affected releases (empty = all).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub versions: String,
    /// `exclude`: matching releases were never selected. `requires`: the
    /// selected release gained `requires`.
    pub effect: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub source: String,
}

/// The `[packages.tool.vyp]` table for one resolved package.
pub fn package_tool(result: &ResolutionResult, name: &str) -> Option<PyLockPackageTool> {
    let record = result.provenance.records.get(name);
    let version = result.packages.get(name);

    let overrides: Vec<LockedOverride> = result
        .overrides
        .iter()
        .filter(|o| vyp_api::normalize_package_name(&o.package) == name)
        .map(|o| LockedOverride {
            constraint: o.constraint.clone(),
            origin: crate::lock::shipped_overrides::describe_origin(o),
            reason: o.reason.clone(),
        })
        .collect();

    let errata: Vec<LockedErratum> = result
        .errata
        .iter()
        .filter(|e| e.package == name)
        .filter_map(|e| {
            let requires_apply = !e.requires.is_empty()
                && version.is_some_and(|v| e.applies_to(name, v));
            let effect = if e.exclude {
                "exclude"
            } else if requires_apply {
                "requires"
            } else {
                return None;
            };
            Some(LockedErratum {
                versions: e.versions_spec.clone(),
                effect: effect.to_string(),
                requires: if effect == "requires" {
                    e.requires.iter().map(requirement_to_string).collect()
                } else {
                    Vec::new()
                },
                reason: e.reason.clone(),
                source: e.source.clone(),
            })
        })
        .collect();

    if record.is_none() && overrides.is_empty() && errata.is_empty() {
        return None;
    }
    let record = record.cloned().unwrap_or_default();
    Some(PyLockPackageTool {
        vyp: Some(VypProvenance {
            selected_by: selection_reason_str(&record.selected_by),
            requested_by: record.requested_by,
            conflict_with: record.conflict_with,
            resolution_path: record.resolution_path,
            overrides,
            errata,
        }),
    })
}

fn requirement_to_string(req: &vyp_api::Requirement) -> String {
    let spec: Vec<String> = req
        .constraints
        .iter()
        .map(|c| format!("{}{}", c.op, c.version))
        .collect();
    format!("{}{}", req.package.name(), spec.join(","))
}

pub fn selection_reason_str(reason: &SelectionReason) -> String {
    match reason {
        SelectionReason::Normal => "normal".to_string(),
        SelectionReason::ConflictResolution => "conflict-resolution".to_string(),
        SelectionReason::Override => "override".to_string(),
        SelectionReason::Substitution => "substitution".to_string(),
        SelectionReason::PluginStrategy(s) => format!("plugin:{}", s),
    }
}

impl PyLockFile {
    /// Create a PEP 751 lock file from a resolution result.
    pub fn from_resolution(result: &ResolutionResult) -> Self {
        let mut sorted_packages: Vec<_> = result.packages.iter().collect();
        sorted_packages.sort_by_key(|(name, _)| (*name).clone());

        let packages: Vec<PyLockPackage> = sorted_packages
            .into_iter()
            .map(|(name, version)| {
                let tool = package_tool(result, name);

                let wheels = result.wheel_urls.get(name.as_str())
                    .map(|info| vec![PyLockWheel {
                        name: info.filename.clone(),
                        url: Some(info.url.clone()),
                        size: None,
                        hashes: if info.hashes.is_empty() {
                            None
                        } else {
                            Some(info.hashes.clone())
                        },
                    }])
                    .unwrap_or_default();

                PyLockPackage {
                    name: name.clone(),
                    version: version.to_string(),
                    requires_python: None,
                    dependencies: Vec::new(),
                    wheels,
                    sdist: None,
                    marker: None,
                    index: None,
                    variant: None,
                    tool,
                }
            })
            .collect();

        PyLockFile {
            lock_version: "1.0".to_string(),
            created_by: format!("vyp {}", env!("CARGO_PKG_VERSION")),
            requires_python: None,
            environments: Vec::new(),
            extras: Vec::new(),
            dependency_groups: Vec::new(),
            default_groups: Vec::new(),
            packages,
            tool: PyLockTool::from_feeds(&result.errata_feeds),
        }
    }

    /// Create a lock file from a universal (multi-environment) resolution.
    pub fn from_universal_resolution(
        entries: &[UniversalPackageEntry],
        environments: &[String],
        requires_python: Option<&str>,
        errata_feeds: &[ErrataFeedDigest],
    ) -> Self {
        let packages: Vec<PyLockPackage> = entries
            .iter()
            .map(|e| PyLockPackage {
                name: e.name.clone(),
                version: e.version.clone(),
                requires_python: None,
                dependencies: Vec::new(),
                wheels: e.wheels.clone(),
                sdist: None,
                marker: e.marker.clone(),
                index: None,
                variant: None,
                tool: e.tool.clone(),
            })
            .collect();

        PyLockFile {
            lock_version: "1.0".to_string(),
            created_by: format!("vyp {}", env!("CARGO_PKG_VERSION")),
            requires_python: requires_python.map(String::from),
            environments: environments.to_vec(),
            extras: Vec::new(),
            dependency_groups: Vec::new(),
            default_groups: Vec::new(),
            packages,
            tool: PyLockTool::from_feeds(errata_feeds),
        }
    }

    /// Write the lock file to a path.
    pub fn write_to_file(&self, path: &Path) -> miette::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| miette::miette!("Failed to serialize pylock.toml: {}", e))?;
        std::fs::write(path, content)
            .map_err(|e| miette::miette!("Failed to write pylock.toml: {}", e))?;
        Ok(())
    }

    /// Read a lock file from a path.
    pub fn read_from_file(path: &Path) -> miette::Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| miette::miette!("Failed to read pylock.toml: {}", e))?;
        toml::from_str(&content)
            .map_err(|e| miette::miette!("Failed to parse pylock.toml: {}", e))
    }

    /// Derive the default file name for a lock file.
    /// Uses `pylock.toml` for default, `pylock.<name>.toml` for named.
    pub fn default_filename(name: Option<&str>) -> String {
        match name {
            Some(n) => format!("pylock.{}.toml", n),
            None => "pylock.toml".to_string(),
        }
    }

    /// Explain why a particular package was selected.
    pub fn explain_package(&self, name: &str) -> Option<String> {
        let pkg = self.packages.iter().find(|p| p.name == name)?;
        let mut explanation = format!("{} == {}\n", pkg.name, pkg.version);

        if let Some(ref tool) = pkg.tool {
            if let Some(ref prov) = tool.vyp {
                explanation.push_str(&format!("  Selected by: {}\n", prov.selected_by));
                if !prov.requested_by.is_empty() {
                    explanation.push_str(&format!(
                        "  Requested by: {}\n",
                        prov.requested_by.join(", ")
                    ));
                }
                if !prov.conflict_with.is_empty() {
                    explanation.push_str(&format!(
                        "  Conflicts with: {}\n",
                        prov.conflict_with.join(", ")
                    ));
                }
                if let Some(path) = &prov.resolution_path {
                    explanation.push_str(&format!("  Resolution path: {}\n", path));
                }
                for o in &prov.overrides {
                    explanation.push_str(&format!(
                        "  Override: every requirement on {} replaced with {} (from {}){}\n",
                        pkg.name,
                        o.constraint,
                        o.origin,
                        o.reason.as_deref().map(|r| format!(" -- {}", r)).unwrap_or_default()
                    ));
                }
                for e in &prov.errata {
                    let versions = if e.versions.is_empty() { "all releases" } else { e.versions.as_str() };
                    let effect = match e.effect.as_str() {
                        "exclude" => format!("releases {} excluded", versions),
                        _ => format!("this release ({}) also requires {}", versions, e.requires.join(", ")),
                    };
                    explanation.push_str(&format!(
                        "  Erratum: {} [{}]{}\n",
                        effect,
                        e.source,
                        e.reason.as_deref().map(|r| format!(" -- {}", r)).unwrap_or_default()
                    ));
                }
            }
        }

        Some(explanation)
    }
}

// Keep the old LockFile type as an alias for backwards compatibility during migration
pub type LockFile = PyLockFile;
