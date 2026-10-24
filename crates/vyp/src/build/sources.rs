//! Path, editable and git dependencies.
//!
//! Declared either as PEP 508 direct references in the dependency list
//! (`pkg @ file:///abs/path`, `pkg @ git+https://host/repo@rev`) or in
//! `[tool.vyp.sources]` (`pkg = [{ path = "../pkg", editable = true }]`,
//! `pkg = [{ git = "https://host/repo", rev = "v1.2" }]`). Before resolving,
//! each source tree's metadata is read — statically from `pyproject.toml`
//! when its `[project]` table allows, otherwise from the build backend — and
//! [`DirectProvider`] offers exactly that version to the resolver, ahead of
//! every index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use vyp_api::{
    normalize_package_name, ConflictSet, MarkerEnvironment, MetadataProvider, PackageMetadata,
    PackageVersions, VypPackage, VypVersion,
};
use vyp_core::SourceRef;

use super::git::{self, GitRef};
use super::Frontend;

/// A source declared in configuration, before it is fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSource {
    Path { path: PathBuf, editable: bool },
    Git(GitRef),
}

impl LocalSource {
    /// A PEP 508 direct reference (`file://` or `git+`), if it is one we build.
    pub fn from_direct_reference(url: &str, project_dir: &Path) -> Option<Self> {
        if let Some(git) = GitRef::parse(url) {
            return Some(Self::Git(git));
        }
        let path = url.strip_prefix("file://")?;
        let path = PathBuf::from(path);
        let path = if path.is_absolute() { path } else { project_dir.join(path) };
        (path.is_dir()).then_some(Self::Path { path, editable: false })
    }
}

/// A fetched source tree with its metadata.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub name: String,
    pub version: VypVersion,
    /// Core metadata (`METADATA`) text.
    pub metadata: String,
    pub reference: SourceRef,
    /// Where the source tree is on disk.
    pub tree: PathBuf,
}

/// Fetch every source and read its metadata.
pub fn resolve_all(
    sources: &[(String, LocalSource)],
    project_dir: &Path,
    frontend: impl Fn() -> miette::Result<Frontend>,
) -> miette::Result<Vec<ResolvedSource>> {
    let mut out = Vec::new();
    for (declared, source) in sources {
        let (tree, reference) = match source {
            LocalSource::Path { path, editable } => {
                let tree = std::fs::canonicalize(path).map_err(|e| {
                    miette::miette!("Source for {} not found at {}: {}", declared, path.display(), e)
                })?;
                let base = std::fs::canonicalize(project_dir).unwrap_or_else(|_| project_dir.to_path_buf());
                let rel = crate::cache::linker::relative_to(&base, &tree);
                (tree, SourceRef::Directory { path: rel, editable: *editable })
            }
            LocalSource::Git(git_ref) => {
                let commit = git::resolve_commit(&git_ref.url, git_ref.rev.as_deref())?;
                let checkout = git::checkout(&git_ref.url, &commit)?;
                let tree = match &git_ref.subdirectory {
                    Some(sub) => checkout.join(sub),
                    None => checkout,
                };
                (
                    tree,
                    SourceRef::Git {
                        url: git_ref.url.clone(),
                        requested_revision: git_ref.rev.clone(),
                        commit,
                        subdirectory: git_ref.subdirectory.clone(),
                    },
                )
            }
        };
        let metadata = match static_metadata(&tree)? {
            Some(m) => m,
            None => frontend()?.metadata(&tree)?,
        };
        let (name, version) = name_and_version(&metadata)
            .ok_or_else(|| miette::miette!("Metadata for {} has no name or version", tree.display()))?;
        if normalize_package_name(&name) != normalize_package_name(declared) {
            return Err(miette::miette!(
                "The source declared for {} at {} is the package {}",
                declared,
                tree.display(),
                name
            ));
        }
        let version = version
            .parse()
            .map_err(|_| miette::miette!("{} has an invalid version '{}'", name, version))?;
        out.push(ResolvedSource { name: normalize_package_name(&name), version, metadata, reference, tree });
    }
    Ok(out)
}

fn name_and_version(metadata: &str) -> Option<(String, String)> {
    let mut name = None;
    let mut version = None;
    for line in metadata.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Name:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Version:") {
            version = Some(v.trim().to_string());
        }
    }
    Some((name?, version?))
}

/// METADATA synthesized from `[project]`, when nothing a build needs to
/// compute (`version`, `dependencies`, `optional-dependencies`) is dynamic.
pub fn static_metadata(tree: &Path) -> miette::Result<Option<String>> {
    let Ok(content) = std::fs::read_to_string(tree.join("pyproject.toml")) else { return Ok(None) };
    let doc: toml::Value = toml::from_str(&content)
        .map_err(|e| miette::miette!("Failed to parse {}/pyproject.toml: {}", tree.display(), e))?;
    let Some(project) = doc.get("project") else { return Ok(None) };
    let dynamic: Vec<&str> = project
        .get("dynamic")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if ["version", "dependencies", "optional-dependencies"].iter().any(|k| dynamic.contains(k)) {
        return Ok(None);
    }
    let (Some(name), Some(version)) = (
        project.get("name").and_then(|v| v.as_str()),
        project.get("version").and_then(|v| v.as_str()),
    ) else {
        return Ok(None);
    };

    let mut out = format!("Metadata-Version: 2.1\nName: {}\nVersion: {}\n", name, version);
    if let Some(rp) = project.get("requires-python").and_then(|v| v.as_str()) {
        out.push_str(&format!("Requires-Python: {}\n", rp));
    }
    for dep in project.get("dependencies").and_then(|d| d.as_array()).into_iter().flatten() {
        if let Some(dep) = dep.as_str() {
            out.push_str(&format!("Requires-Dist: {}\n", dep));
        }
    }
    if let Some(extras) = project.get("optional-dependencies").and_then(|d| d.as_table()) {
        for (extra, deps) in extras {
            out.push_str(&format!("Provides-Extra: {}\n", extra));
            for dep in deps.as_array().into_iter().flatten().filter_map(|d| d.as_str()) {
                let line = match dep.split_once(';') {
                    Some((req, marker)) => format!("{} ; ({}) and extra == \"{}\"", req.trim(), marker.trim(), extra),
                    None => format!("{} ; extra == \"{}\"", dep.trim(), extra),
                };
                out.push_str(&format!("Requires-Dist: {}\n", line));
            }
        }
    }
    Ok(Some(out))
}

/// Serves resolved source trees to the resolver, ahead of every index.
#[derive(Debug)]
pub struct DirectProvider {
    packages: HashMap<String, PackageMetadata>,
}

impl DirectProvider {
    pub fn new(sources: &[ResolvedSource], env: &MarkerEnvironment) -> Self {
        let packages = sources
            .iter()
            .map(|s| {
                let deps = vyp_index::pypi::parse_metadata_requires_dist(&s.metadata, env);
                (
                    s.name.clone(),
                    PackageMetadata {
                        package: VypPackage::named(&s.name),
                        version: s.version.clone(),
                        dependencies: deps,
                        conflict_declarations: ConflictSet::default(),
                        source: s.tree.display().to_string(),
                    },
                )
            })
            .collect();
        Self { packages }
    }
}

impl MetadataProvider for DirectProvider {
    fn name(&self) -> &str {
        "local-sources"
    }

    fn priority(&self) -> i32 {
        1000
    }

    fn can_provide(&self, package: &VypPackage) -> bool {
        matches!(package, VypPackage::Named(_)) && self.packages.contains_key(package.name())
    }

    fn available_versions(
        &self,
        package: &VypPackage,
    ) -> Result<Option<PackageVersions>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.packages.get(package.name()).map(|m| PackageVersions {
            package: package.clone(),
            versions: vec![m.version.clone()],
        }))
    }

    fn try_available_versions(&self, package: &VypPackage) -> Option<PackageVersions> {
        self.available_versions(package).ok().flatten()
    }

    fn get_metadata(
        &self,
        package: &VypPackage,
        version: &VypVersion,
    ) -> Result<Option<PackageMetadata>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.packages.get(package.name()).filter(|m| &m.version == version).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pyproject: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vyp-src-{}-{}", std::process::id(), pyproject.len()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pyproject.toml"), pyproject).unwrap();
        dir
    }

    #[test]
    fn static_metadata_from_project_table() {
        let dir = tree(
            "[project]\nname = \"Lib-B\"\nversion = \"0.3\"\nrequires-python = \">=3.9\"\ndependencies = [\"requests>=2\"]\n\
             [project.optional-dependencies]\nfast = [\"orjson; sys_platform != 'win32'\"]\n",
        );
        let md = static_metadata(&dir).unwrap().unwrap();
        assert!(md.contains("Name: Lib-B\nVersion: 0.3\n"), "{}", md);
        assert!(md.contains("Requires-Dist: requests>=2\n"));
        assert!(md.contains("Requires-Dist: orjson ; (sys_platform != 'win32') and extra == \"fast\"\n"), "{}", md);
        assert_eq!(name_and_version(&md), Some(("Lib-B".into(), "0.3".into())));

        let dynamic = tree("[project]\nname = \"x\"\ndynamic = [\"version\"]\n");
        assert!(static_metadata(&dynamic).unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dynamic);
    }

    #[test]
    fn direct_references() {
        let here = std::env::temp_dir();
        assert!(matches!(
            LocalSource::from_direct_reference("git+https://github.com/o/r@v1", &here),
            Some(LocalSource::Git(_))
        ));
        assert!(matches!(
            LocalSource::from_direct_reference(&format!("file://{}", here.display()), &here),
            Some(LocalSource::Path { editable: false, .. })
        ));
        assert_eq!(LocalSource::from_direct_reference("https://example.com/x.whl", &here), None);
    }

    #[test]
    fn provider_offers_exactly_the_source_version() {
        let source = ResolvedSource {
            name: "lib_b".into(),
            version: "0.3".parse().unwrap(),
            metadata: "Metadata-Version: 2.1\nName: lib-b\nVersion: 0.3\nRequires-Dist: requests\n".into(),
            reference: SourceRef::Directory { path: "../lib-b".into(), editable: true },
            tree: PathBuf::from("/x"),
        };
        let provider = DirectProvider::new(&[source], &MarkerEnvironment::current());
        let pkg = VypPackage::named("lib-b");
        assert!(provider.can_provide(&pkg));
        let versions = provider.available_versions(&pkg).unwrap().unwrap().versions;
        assert_eq!(versions.len(), 1);
        let md = provider.get_metadata(&pkg, &versions[0]).unwrap().unwrap();
        assert_eq!(md.dependencies[0].package.name(), "requests");
    }
}
