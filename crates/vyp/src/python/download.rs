//! Installing managed CPython builds from python-build-standalone.
//!
//! Releases are listed through the GitHub API (override the base URL with
//! `VYP_PYTHON_DOWNLOADS_URL`, e.g. for a mirror). vyp picks the newest
//! `install_only_stripped` (or `install_only`) archive for this platform that
//! satisfies the request, verifies its SHA-256 against the release's
//! published digest, and unpacks it under [`super::managed_dir`].

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{managed_dir, probe, version_satisfies, Interpreter, PythonRequest};

const DEFAULT_API: &str = "https://api.github.com";
const REPO: &str = "astral-sh/python-build-standalone";
/// Release pages (newest first) to search before giving up.
const MAX_PAGES: u32 = 4;

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize, Clone)]
struct Asset {
    name: String,
    browser_download_url: String,
    /// `sha256:<hex>`, published by GitHub for release assets.
    #[serde(default)]
    digest: Option<String>,
}

/// A downloadable build matching this platform.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub version: String,
    pub name: String,
    pub url: String,
    digest: Option<String>,
    sums_url: Option<String>,
    stripped: bool,
}

/// The python-build-standalone target triple for this machine.
pub fn platform_triple() -> Option<&'static str> {
    let arch = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    let triple = match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") if cfg!(target_env = "musl") => "x86_64-unknown-linux-musl",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") if cfg!(target_env = "musl") => "aarch64-unknown-linux-musl",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        _ => return None,
    };
    Some(triple)
}

/// Parse `cpython-3.12.7+20241016-aarch64-apple-darwin-install_only_stripped.tar.gz`
/// into `(version, triple, stripped)`.
fn parse_asset_name(name: &str) -> Option<(String, String, bool)> {
    let rest = name.strip_prefix("cpython-")?.strip_suffix(".tar.gz")?;
    let (version, rest) = rest.split_once('+')?;
    let (_, rest) = rest.split_once('-')?; // build date
    let (triple, stripped) = if let Some(t) = rest.strip_suffix("-install_only_stripped") {
        (t, true)
    } else {
        (rest.strip_suffix("-install_only")?, false)
    };
    if version.contains(['a', 'b', 'r']) {
        return None; // skip pre-releases
    }
    Some((version.to_string(), triple.to_string(), stripped))
}

fn api_base() -> String {
    std::env::var("VYP_PYTHON_DOWNLOADS_URL").unwrap_or_else(|_| DEFAULT_API.to_string())
}

fn client() -> miette::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("vyp/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| miette::miette!("Failed to create HTTP client: {}", e))
}

fn get(client: &reqwest::blocking::Client, url: &str) -> reqwest::blocking::RequestBuilder {
    let req = client.get(url);
    match std::env::var("GITHUB_TOKEN") {
        Ok(token) if url.starts_with(DEFAULT_API) => req.bearer_auth(token),
        _ => req,
    }
}

/// The newest build for this platform satisfying the request.
pub fn find_candidate(
    request: &PythonRequest,
    requires_python: Option<&str>,
) -> miette::Result<Candidate> {
    let triple = platform_triple().ok_or_else(|| {
        miette::miette!("Managed Python downloads aren't available for this platform")
    })?;
    if let PythonRequest::Version {
        free_threaded: true,
        ..
    } = request
    {
        return Err(miette::miette!(
            "Downloading free-threaded builds isn't supported yet"
        ));
    }
    let client = client()?;
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/repos/{}/releases?per_page=5&page={}",
            api_base(),
            REPO,
            page
        );
        let releases: Vec<Release> = get(&client, &url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json())
            .map_err(|e| miette::miette!("Failed to list Python builds from {}: {}", url, e))?;
        if releases.is_empty() {
            break;
        }
        if let Some(found) = best_in(&releases, triple, request, requires_python) {
            return Ok(found);
        }
    }
    Err(miette::miette!(
        "No downloadable Python build for {} matches {}",
        triple,
        super::describe_request(request)
    ))
}

fn best_in(
    releases: &[Release],
    triple: &str,
    request: &PythonRequest,
    requires_python: Option<&str>,
) -> Option<Candidate> {
    let mut best: Option<(vyp_api::VypVersion, Candidate)> = None;
    for release in releases {
        let sums_url = release
            .assets
            .iter()
            .find(|a| a.name == "SHA256SUMS")
            .map(|a| a.browser_download_url.clone());
        for asset in &release.assets {
            let Some((version, asset_triple, stripped)) = parse_asset_name(&asset.name) else {
                continue;
            };
            if asset_triple != triple {
                continue;
            }
            let probe_like = Interpreter {
                executable: PathBuf::new(),
                base_executable: PathBuf::new(),
                version: version.clone(),
                implementation: "cpython".into(),
                is_venv: false,
                free_threaded: false,
                sys_platform: String::new(),
                machine: String::new(),
                purelib: String::new(),
                scripts: String::new(),
                include: String::new(),
                managed: true,
            };
            if !request.matches(&probe_like)
                || requires_python.is_some_and(|rp| !version_satisfies(&version, rp))
            {
                continue;
            }
            let Ok(parsed) = version.parse::<vyp_api::VypVersion>() else {
                continue;
            };
            let candidate = Candidate {
                version,
                name: asset.name.clone(),
                url: asset.browser_download_url.clone(),
                digest: asset.digest.clone(),
                sums_url: sums_url.clone(),
                stripped,
            };
            let better = match &best {
                None => true,
                Some((v, c)) => parsed > *v || (parsed == *v && stripped && !c.stripped),
            };
            if better {
                best = Some((parsed, candidate));
            }
        }
        let _ = &release.tag_name;
    }
    best.map(|(_, c)| c)
}

/// Download, verify and unpack a matching build; return its interpreter.
pub fn install(
    request: &PythonRequest,
    requires_python: Option<&str>,
) -> miette::Result<Interpreter> {
    let candidate = find_candidate(request, requires_python)?;
    install_candidate(&candidate)
}

pub fn install_dir_for(version: &str) -> PathBuf {
    managed_dir().join(format!(
        "cpython-{}-{}",
        version,
        platform_triple().unwrap_or("unknown")
    ))
}

pub fn install_candidate(candidate: &Candidate) -> miette::Result<Interpreter> {
    let dest = install_dir_for(&candidate.version);
    if let Some(interp) = super::managed_executable(&dest).and_then(|exe| probe(&exe)) {
        return Ok(interp);
    }

    let client = client()?;
    let expected = expected_sha256(&client, candidate)?;

    eprintln!("Downloading {}", candidate.name);
    let mut resp = get(&client, &candidate.url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| miette::miette!("Failed to download {}: {}", candidate.url, e))?;
    let mut bytes = Vec::new();
    resp.read_to_end(&mut bytes)
        .map_err(|e| miette::miette!("Failed to download {}: {}", candidate.url, e))?;

    let actual = super::hex_sha256(&bytes);
    if actual != expected {
        return Err(miette::miette!(
            "Checksum mismatch for {}: expected {}, got {}",
            candidate.name,
            expected,
            actual
        ));
    }

    let root = managed_dir();
    std::fs::create_dir_all(&root)
        .map_err(|e| miette::miette!("Failed to create {}: {}", root.display(), e))?;
    let staging = root.join(format!(".tmp-{}-{}", candidate.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    unpack_tar_gz(&bytes, &staging)?;

    // Archives hold a single top-level `python/` directory.
    let unpacked = staging.join("python");
    if !unpacked.is_dir() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(miette::miette!(
            "Unexpected archive layout in {}",
            candidate.name
        ));
    }
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&unpacked, &dest)
        .map_err(|e| miette::miette!("Failed to install to {}: {}", dest.display(), e))?;
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::write(
        dest.join("vyp-install.json"),
        serde_json::json!({ "source": candidate.url, "sha256": expected }).to_string(),
    );

    let exe = super::managed_executable(&dest)
        .ok_or_else(|| miette::miette!("No interpreter found in {}", dest.display()))?;
    let interp = probe(&exe)
        .ok_or_else(|| miette::miette!("Installed interpreter at {} doesn't run", exe.display()))?;
    eprintln!("Installed {}", interp.describe());
    Ok(interp)
}

fn expected_sha256(
    client: &reqwest::blocking::Client,
    candidate: &Candidate,
) -> miette::Result<String> {
    if let Some(hex) = candidate
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        return Ok(hex.to_lowercase());
    }
    let sums_url = candidate.sums_url.as_ref().ok_or_else(|| {
        miette::miette!(
            "No published checksum for {}; refusing to install it",
            candidate.name
        )
    })?;
    let sums = get(client, sums_url)
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.text())
        .map_err(|e| miette::miette!("Failed to fetch checksums from {}: {}", sums_url, e))?;
    sums.lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == candidate.name)
        .map(|(hex, _)| hex.trim().to_lowercase())
        .ok_or_else(|| {
            miette::miette!(
                "{} isn't listed in {}; refusing to install it",
                candidate.name,
                sums_url
            )
        })
}

/// Unpack a `.tar.gz`, refusing entries that escape `dest`.
pub fn unpack_tar_gz(bytes: &[u8], dest: &Path) -> miette::Result<()> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive.set_preserve_permissions(true);
    std::fs::create_dir_all(dest)
        .map_err(|e| miette::miette!("Failed to create {}: {}", dest.display(), e))?;
    for entry in archive
        .entries()
        .map_err(|e| miette::miette!("Corrupt archive: {}", e))?
    {
        let mut entry = entry.map_err(|e| miette::miette!("Corrupt archive: {}", e))?;
        // `unpack_in` rejects absolute paths and `..` components.
        if !entry
            .unpack_in(dest)
            .map_err(|e| miette::miette!("Failed to unpack archive: {}", e))?
        {
            return Err(miette::miette!(
                "Archive entry escapes the install directory"
            ));
        }
    }
    Ok(())
}

/// Remove a managed install.
pub fn uninstall(version: &str) -> miette::Result<Vec<PathBuf>> {
    let request = PythonRequest::parse(version);
    let mut removed = Vec::new();
    for (dir, exe) in super::managed_installs() {
        let Some(interp) = probe(&exe) else { continue };
        if request.matches(&interp) {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| miette::miette!("Failed to remove {}: {}", dir.display(), e))?;
            removed.push(dir);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            browser_download_url: format!("https://example.com/{}", name),
            digest: Some("sha256:abc".into()),
        }
    }

    #[test]
    fn parses_asset_names() {
        assert_eq!(
            parse_asset_name(
                "cpython-3.12.7+20241016-aarch64-apple-darwin-install_only_stripped.tar.gz"
            ),
            Some(("3.12.7".into(), "aarch64-apple-darwin".into(), true))
        );
        assert_eq!(
            parse_asset_name(
                "cpython-3.11.10+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz"
            ),
            Some(("3.11.10".into(), "x86_64-unknown-linux-gnu".into(), false))
        );
        assert_eq!(
            parse_asset_name("cpython-3.14.0a1+20241016-aarch64-apple-darwin-install_only.tar.gz"),
            None
        );
        assert_eq!(
            parse_asset_name("cpython-3.12.7+20241016-aarch64-apple-darwin-pgo+lto-full.tar.zst"),
            None
        );
    }

    #[test]
    fn picks_newest_matching_and_prefers_stripped() {
        let t = "aarch64-apple-darwin";
        let releases = vec![Release {
            tag_name: "20241016".into(),
            assets: vec![
                asset(&format!(
                    "cpython-3.12.7+20241016-{}-install_only.tar.gz",
                    t
                )),
                asset(&format!(
                    "cpython-3.12.7+20241016-{}-install_only_stripped.tar.gz",
                    t
                )),
                asset(&format!(
                    "cpython-3.13.0+20241016-{}-install_only.tar.gz",
                    t
                )),
                asset("cpython-3.12.7+20241016-x86_64-unknown-linux-gnu-install_only.tar.gz"),
            ],
        }];
        let got = best_in(&releases, t, &PythonRequest::parse("3.12"), None).unwrap();
        assert_eq!(got.version, "3.12.7");
        assert!(got.stripped);
        let any = best_in(&releases, t, &PythonRequest::Any, Some(">=3.10")).unwrap();
        assert_eq!(any.version, "3.13.0");
        assert!(best_in(&releases, t, &PythonRequest::parse("3.9"), None).is_none());
    }

    #[test]
    fn unpack_rejects_path_traversal() {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let data = b"x";
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        // `append_data` validates paths, so write the raw name directly.
        header.as_gnu_mut().unwrap().name[..12].copy_from_slice(b"../evil.txt\0");
        header.set_cksum();
        builder.append(&header, &data[..]).unwrap();
        let bytes = builder.into_inner().unwrap().finish().unwrap();
        let dir = std::env::temp_dir().join(format!("vyp-untar-{}", std::process::id()));
        assert!(
            unpack_tar_gz(&bytes, &dir).is_err()
                || !dir.parent().unwrap().join("evil.txt").exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
