//! Overrides that dependencies ship inside their wheels.
//!
//! A library that knows how a conflict in its dependency tree must be resolved
//! records it with `transitive = true` and runs `vyp override export`, then
//! includes the resulting `vyp-overrides.toml` in its wheel's `.dist-info`
//! directory. When that release is selected, vyp reads the file with HTTP
//! range requests (a few KiB, never the whole wheel) and applies the
//! overrides to the consumer's resolution.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use vyp_api::{normalize_package_name, DependencyOverride, Requirement};
use vyp_core::resolver::requirements_to_range;
use vyp_core::ResolutionResult;

use crate::cache::wheel_cache::cache_base_dir;
use crate::config::settings::InheritOverrides;

use super::conflict_overrides::OverridesExportFile;

pub const FILE_NAME: &str = "vyp-overrides.toml";

const MAX_CONCURRENT_FETCHES: usize = 32;

type Fetch = tokio::task::JoinHandle<Option<String>>;

/// Starts reading a release's shipped overrides as soon as the solver
/// selects it, so the reads overlap resolution instead of following it.
/// Reads for releases the solver later backtracks away from are cached, not
/// wasted.
#[derive(Clone)]
pub struct Prefetcher {
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    policy: InheritOverrides,
    in_flight: Arc<Mutex<HashMap<PathBuf, Fetch>>>,
}

impl Prefetcher {
    pub fn new(client: reqwest::Client, policy: InheritOverrides) -> Self {
        Self {
            client,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_FETCHES)),
            policy,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Begin reading the wheel's overrides in the background, unless they
    /// are already being read or not allowed by the policy. Runs on the
    /// solver thread, so it does no I/O itself.
    pub fn start(&self, package: &str, url: &str, sha256: Option<&str>) {
        if !self.policy.allows(package) {
            return;
        }
        let cached = cache_path(url, sha256);
        let mut in_flight = self.in_flight.lock().expect("poisoned");
        if in_flight.contains_key(&cached) {
            return;
        }
        let task = vyp_index::pypi::shared_runtime().spawn(fetch_one(
            self.client.clone(),
            Arc::clone(&self.semaphore),
            url.to_string(),
            cached.clone(),
        ));
        in_flight.insert(cached, task);
    }

    fn take(&self, cached: &PathBuf) -> Option<Fetch> {
        self.in_flight.lock().expect("poisoned").remove(cached)
    }
}

/// Read the overrides shipped by every selected release the policy allows,
/// waiting for reads the prefetcher started during resolution.
pub fn collect(
    result: &ResolutionResult,
    prefetcher: &Prefetcher,
) -> miette::Result<Vec<DependencyOverride>> {
    let mut selected: Vec<_> = result.packages.iter().collect();
    selected.sort_by(|a, b| a.0.cmp(b.0));

    let mut contents: Vec<(String, String, Option<String>)> = Vec::new();
    let mut pending: Vec<(String, String, Fetch)> = Vec::new();
    for (name, version) in selected {
        if !prefetcher.policy.allows(name) {
            continue;
        }
        let Some(wheel) = result.wheel_urls.get(name) else {
            continue;
        };
        let cached = cache_path(&wheel.url, wheel.hashes.get("sha256").map(String::as_str));
        if let Some(task) = prefetcher.take(&cached) {
            pending.push((name.clone(), version.to_string(), task));
            continue;
        }
        match std::fs::read_to_string(&cached) {
            Ok(body) => contents.push((
                name.clone(),
                version.to_string(),
                Some(body).filter(|b| !b.is_empty()),
            )),
            Err(_) => pending.push((
                name.clone(),
                version.to_string(),
                vyp_index::pypi::shared_runtime().spawn(fetch_one(
                    prefetcher.client.clone(),
                    Arc::clone(&prefetcher.semaphore),
                    wheel.url.clone(),
                    cached,
                )),
            )),
        }
    }

    if !pending.is_empty() {
        let (meta, tasks): (Vec<_>, Vec<_>) =
            pending.into_iter().map(|(n, v, t)| ((n, v), t)).unzip();
        let bodies = vyp_index::pypi::shared_runtime().block_on(futures::future::join_all(tasks));
        for ((name, version), body) in meta.into_iter().zip(bodies) {
            contents.push((name, version, body.unwrap_or(None)));
        }
    }

    let mut inherited = Vec::new();
    for (name, version, body) in contents {
        let Some(body) = body else { continue };
        let file = match OverridesExportFile::parse(&body) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(
                    "ignoring malformed {} in {}=={}: {}",
                    FILE_NAME,
                    name,
                    version,
                    e
                );
                continue;
            }
        };
        if normalize_package_name(&file.package) != name {
            tracing::warn!(
                "ignoring {} in {}=={}: it was exported for package '{}'",
                FILE_NAME,
                name,
                version,
                file.package
            );
            continue;
        }
        inherited.extend(file.inherited_overrides(&format!("{}=={}", name, version)));
    }
    Ok(inherited)
}

fn cache_path(url: &str, sha256: Option<&str>) -> PathBuf {
    let key = sha256.map_or_else(|| hex_sha256(url.as_bytes()), str::to_string);
    cache_base_dir()
        .join("shipped-overrides")
        .join(format!("{}.toml", key))
}

async fn fetch_one(
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    url: String,
    cached: PathBuf,
) -> Option<String> {
    // An empty cache file records that the wheel ships no overrides.
    if let Ok(body) = std::fs::read_to_string(&cached) {
        return Some(body).filter(|b| !b.is_empty());
    }
    let fetched = vyp_index::wheel_metadata::fetch_wheel_member(
        &client,
        &url,
        &semaphore,
        vyp_index::wheel_metadata::SMALL_TAIL_BYTES,
        |member| vyp_index::wheel_metadata::is_dist_info_file(member, FILE_NAME),
    )
    .await;
    match fetched {
        Ok(body) => {
            if let Some(dir) = cached.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cached, body.as_deref().unwrap_or(""));
            body
        }
        Err(e) => {
            // Not cached: a transient failure must not hide an override on
            // the next run.
            tracing::warn!("could not read {} from {}: {}", FILE_NAME, url, e);
            None
        }
    }
}

/// Combine the project's overrides with inherited ones. The project's own
/// override on a package always wins; inherited overrides on the same package
/// from different releases must be compatible.
pub fn effective_overrides(
    project: &[DependencyOverride],
    inherited: &[DependencyOverride],
) -> miette::Result<Vec<DependencyOverride>> {
    let project_packages: BTreeSet<String> = project
        .iter()
        .map(|o| normalize_package_name(&o.package))
        .collect();

    let mut by_package: HashMap<String, Vec<&DependencyOverride>> = HashMap::new();
    for o in inherited {
        let name = normalize_package_name(&o.package);
        if project_packages.contains(&name) {
            continue;
        }
        let group = by_package.entry(name).or_default();
        if !group
            .iter()
            .any(|g| g.constraint == o.constraint && g.origin == o.origin)
        {
            group.push(o);
        }
    }

    let mut out = project.to_vec();
    let mut names: Vec<_> = by_package.keys().cloned().collect();
    names.sort();
    for name in names {
        let group = &by_package[&name];
        let mut range = vyp_core::resolver::VS::full();
        for o in group {
            let req: Requirement =
                format!("{} {}", o.package, o.constraint)
                    .parse()
                    .map_err(|_| {
                        miette::miette!(
                            "Invalid inherited override for {}: '{}'",
                            o.package,
                            o.constraint
                        )
                    })?;
            range = range.intersection(&requirements_to_range(&req.constraints));
        }
        if range.is_empty() {
            let parts: Vec<String> = group
                .iter()
                .map(|o| {
                    format!(
                        "{} {} (from {})",
                        o.package,
                        o.constraint,
                        describe_origin(o)
                    )
                })
                .collect();
            return Err(miette::miette!(
                "Dependencies ship incompatible overrides for {}:\n  {}\nAdd a [[tool.vyp.overrides]] entry for {} to decide.",
                name,
                parts.join("\n  "),
                name
            ));
        }
        out.extend(group.iter().map(|o| (*o).clone()));
    }
    Ok(out)
}

/// `lib-a==1.0` or, for an override inherited further, `libx==2 via lib-a==1.0`.
pub fn describe_origin(o: &DependencyOverride) -> String {
    let origin = o.origin.as_deref().unwrap_or("pyproject.toml");
    match o.propagation_path.last() {
        Some(via) if via != origin => format!("{} via {}", origin, via),
        _ => origin.to_string(),
    }
}

/// Order-insensitive identity of a set of inherited overrides.
pub fn fingerprint(overrides: &[DependencyOverride]) -> BTreeSet<(String, String, String)> {
    overrides
        .iter()
        .map(|o| {
            (
                normalize_package_name(&o.package),
                o.constraint.clone(),
                describe_origin(o),
            )
        })
        .collect()
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inherited(pkg: &str, constraint: &str, via: &str) -> DependencyOverride {
        DependencyOverride::new(pkg, constraint)
            .with_transitive(true)
            .inherit_through(via)
    }

    #[test]
    fn project_override_wins_over_inherited() {
        let project = vec![DependencyOverride::new("numpy", "<2")];
        let out =
            effective_overrides(&project, &[inherited("NumPy", ">=2", "lib-a==1.0")]).unwrap();
        assert_eq!(out, project);
    }

    #[test]
    fn incompatible_inherited_overrides_are_an_error() {
        let err = effective_overrides(
            &[],
            &[
                inherited("numpy", ">=2", "lib-a==1.0"),
                inherited("numpy", "<2", "lib-b==3.1"),
            ],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("lib-a==1.0") && err.contains("lib-b==3.1"),
            "{}",
            err
        );
    }

    #[test]
    fn compatible_inherited_overrides_are_kept() {
        let out = effective_overrides(
            &[],
            &[
                inherited("numpy", ">=1.26", "lib-a==1.0"),
                inherited("numpy", "<2", "lib-b==3.1"),
            ],
        )
        .unwrap();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn shipped_file_round_trips_through_export_format() {
        let exported = OverridesExportFile::from_overrides(
            "lib-a",
            "1.0",
            &[DependencyOverride::new("libx", ">=2")
                .with_transitive(true)
                .with_reason("needs the v2 API")],
        );
        let body = toml::to_string_pretty(&exported).unwrap();
        let parsed = OverridesExportFile::parse(&body).unwrap();
        let got = parsed.inherited_overrides("lib-a==1.0");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].constraint, ">=2");
        assert_eq!(describe_origin(&got[0]), "lib-a==1.0");
    }
}
