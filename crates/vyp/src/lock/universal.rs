//! Universal resolution: merge multiple per-environment resolution results
//! and apply fork-strategy (requires-python vs fewest).

use std::collections::HashSet;
use vyp_core::{ResolutionResult, WheelUrlInfo};

use super::lockfile::{
    directory_ref, package_tool, vcs_ref, PyLockDirectory, PyLockPackageTool, PyLockSdist,
    PyLockVcs, PyLockWheel,
};
use crate::config::settings::ForkStrategy;

/// One package entry in a universal (multi-environment) lockfile.
#[derive(Debug, Clone)]
pub struct UniversalPackageEntry {
    pub name: String,
    pub version: String,
    pub marker: Option<String>,
    pub tool: Option<PyLockPackageTool>,
    pub wheels: Vec<PyLockWheel>,
    pub sdist: Option<PyLockSdist>,
    pub directory: Option<PyLockDirectory>,
    pub vcs: Option<PyLockVcs>,
}

/// Parse a marker string of the form `python_version == "X.Y"` to extract "X.Y".
/// Returns None for unsupported formats (first version supports only this pattern).
pub fn parse_python_version_from_marker(marker: &str) -> Option<String> {
    let marker = marker.trim();
    // python_version == "3.8" or python_version=="3.8"
    let rest = marker
        .strip_prefix("python_version")?
        .trim_start_matches([' ', '=']);
    let rest = rest.trim();
    let quoted = rest.strip_prefix('"')?.strip_suffix('"')?;
    if quoted.contains('"') {
        return None;
    }
    // Basic X.Y check
    let mut parts = quoted.split('.');
    let _major = parts.next()?.parse::<u8>().ok()?;
    let _minor = parts.next()?.parse::<u8>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(quoted.to_string())
}

/// Merge N per-environment resolution results into a single list of package entries
/// according to the given fork strategy.
pub fn merge_universal_results(
    env_results: Vec<(String, ResolutionResult)>,
    fork_strategy: ForkStrategy,
) -> Vec<UniversalPackageEntry> {
    match fork_strategy {
        ForkStrategy::RequiresPython => merge_requires_python(env_results),
        ForkStrategy::Fewest => merge_fewest(env_results),
    }
}

fn merge_requires_python(
    env_results: Vec<(String, ResolutionResult)>,
) -> Vec<UniversalPackageEntry> {
    let mut entries = Vec::new();
    for (marker, result) in env_results {
        for (name, version) in &result.packages {
            let tool = package_tool(&result, name);
            let wheels = result
                .wheel_urls
                .get(name.as_str())
                .map(wheel_info_to_lock_wheels)
                .unwrap_or_default();
            entries.push(UniversalPackageEntry {
                name: name.clone(),
                version: version.to_string(),
                marker: Some(marker.clone()),
                tool,
                wheels,
                sdist: result
                    .sdist_urls
                    .get(name.as_str())
                    .map(|info| PyLockSdist {
                        name: info.filename.clone(),
                        url: Some(info.url.clone()),
                        size: None,
                        hashes: (!info.hashes.is_empty()).then(|| info.hashes.clone()),
                    }),
                directory: directory_ref(&result, name),
                vcs: vcs_ref(&result, name),
            });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.marker.cmp(&b.marker)));
    entries
}

fn merge_fewest(env_results: Vec<(String, ResolutionResult)>) -> Vec<UniversalPackageEntry> {
    if env_results.is_empty() {
        return Vec::new();
    }
    if env_results.len() == 1 {
        let (marker, result) = env_results.into_iter().next().unwrap();
        return merge_requires_python(vec![(marker, result)]);
    }

    // Per package name: list of (version, marker) one per environment.
    // If all envs chose the same version -> one entry with marker None.
    // Else -> one entry per (version, marker).
    let mut entries = Vec::new();

    for (marker, result) in &env_results {
        for (name, version) in &result.packages {
            let version_str = version.to_string();
            let same_version_everywhere = env_results.iter().all(|(_, r)| {
                r.packages.get(name).map(|v| v.to_string()) == Some(version_str.clone())
            });
            let marker_for_entry = if same_version_everywhere {
                None
            } else {
                Some(marker.clone())
            };
            let tool = package_tool(result, name);
            let wheels = result
                .wheel_urls
                .get(name.as_str())
                .map(wheel_info_to_lock_wheels)
                .unwrap_or_default();
            entries.push(UniversalPackageEntry {
                name: name.clone(),
                version: version_str,
                marker: marker_for_entry,
                tool,
                wheels,
                sdist: result
                    .sdist_urls
                    .get(name.as_str())
                    .map(|info| PyLockSdist {
                        name: info.filename.clone(),
                        url: Some(info.url.clone()),
                        size: None,
                        hashes: (!info.hashes.is_empty()).then(|| info.hashes.clone()),
                    }),
                directory: directory_ref(result, name),
                vcs: vcs_ref(result, name),
            });
        }
    }
    // Deduplicate: for fewest we want one entry per (name, version, marker). When same version
    // everywhere we emitted one per env with marker None - keep one.
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert((e.name.clone(), e.version.clone(), e.marker.clone())));
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.marker.cmp(&b.marker)));
    entries
}

fn wheel_info_to_lock_wheels(info: &WheelUrlInfo) -> Vec<PyLockWheel> {
    vec![PyLockWheel {
        name: info.filename.clone(),
        url: Some(info.url.clone()),
        size: None,
        hashes: if info.hashes.is_empty() {
            None
        } else {
            Some(info.hashes.clone())
        },
    }]
}
