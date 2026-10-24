//! Source distributions: download, cache, and read static metadata.
//!
//! An sdist's `PKG-INFO` is trustworthy for resolution when it declares
//! Metadata-Version 2.2+ and doesn't list `Requires-Dist` as `Dynamic`
//! (PEP 643); then no build is needed to learn its dependencies.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Builds METADATA for an sdist whose `PKG-INFO` can't be trusted, by
/// running its build backend. Supplied by the frontend, which knows how to
/// make build environments.
pub type SdistMetadataBuilder = std::sync::Arc<dyn Fn(&Path) -> Result<String, String> + Send + Sync>;

pub fn cache_dir() -> PathBuf {
    std::env::var("VYP_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".cache")
            })
        })
        .join("vyp")
        .join("cache")
        .join("sdists")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Download an sdist into the cache (keyed by its SHA-256), verifying the
/// published hash when there is one. Returns the cached path.
pub async fn download(
    client: &reqwest::Client,
    url: &str,
    filename: &str,
    expected_sha256: Option<&str>,
    semaphore: &Semaphore,
) -> Result<PathBuf, DynError> {
    let key = expected_sha256
        .map(str::to_string)
        .unwrap_or_else(|| hex(&Sha256::digest(url.as_bytes())));
    let dest = cache_dir().join(&key).join(filename);
    if dest.exists() {
        return Ok(dest);
    }
    let _permit = semaphore.acquire().await?;
    let bytes = crate::auth::apply_auth(client.get(url), url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let actual = hex(&Sha256::digest(&bytes));
    if let Some(expected) = expected_sha256 {
        if actual != expected {
            return Err(format!("hash mismatch for {}: expected {}, got {}", filename, expected, actual).into());
        }
    }
    let dir = dest.parent().ok_or("bad cache path")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.tmp{}", filename, std::process::id()));
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// The top-level `PKG-INFO` of an sdist (`<name>-<version>/PKG-INFO`).
pub fn read_pkg_info(path: &Path) -> Result<String, DynError> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let is_top_level_pkg_info = |p: &str| {
        let p = p.trim_start_matches("./");
        let mut parts = p.split('/');
        matches!((parts.next(), parts.next(), parts.next()), (Some(_), Some("PKG-INFO"), None))
    };
    if name.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?)?;
        for i in 0..zip.len() {
            let mut file = zip.by_index(i)?;
            if is_top_level_pkg_info(file.name()) {
                let mut s = String::new();
                file.read_to_string(&mut s)?;
                return Ok(s);
            }
        }
    } else {
        let gz = flate2::read::GzDecoder::new(std::fs::File::open(path)?);
        let mut archive = tar::Archive::new(gz);
        for entry in archive.entries()? {
            let mut entry = entry?;
            let p = entry.path()?.to_string_lossy().to_string();
            if is_top_level_pkg_info(&p) {
                let mut s = String::new();
                entry.read_to_string(&mut s)?;
                return Ok(s);
            }
        }
    }
    Err(format!("no PKG-INFO in {}", name).into())
}

/// Whether `PKG-INFO` declares its dependencies statically (PEP 643).
pub fn has_static_requirements(pkg_info: &str) -> bool {
    let mut version_ok = false;
    for line in pkg_info.lines() {
        if line.is_empty() {
            break; // end of headers; the description follows
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if key == "metadata-version" {
            let mut parts = value.split('.');
            let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            version_ok = (major, minor) >= (2, 2);
        } else if key == "dynamic" && value.eq_ignore_ascii_case("requires-dist") {
            return false;
        }
    }
    version_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_requirement_detection() {
        assert!(has_static_requirements("Metadata-Version: 2.2\nName: x\nRequires-Dist: a\n"));
        assert!(has_static_requirements("Metadata-Version: 2.4\nDynamic: License\n\nbody Dynamic: Requires-Dist"));
        assert!(!has_static_requirements("Metadata-Version: 2.1\nName: x\n"));
        assert!(!has_static_requirements("Metadata-Version: 2.3\nDynamic: Requires-Dist\n"));
    }

    #[test]
    fn reads_pkg_info_from_tarball() {
        let dir = std::env::temp_dir().join(format!("vyp-sdist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("demo-1.0.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, body) in [("demo-1.0/PKG-INFO", "Metadata-Version: 2.2\nName: demo\n"), ("demo-1.0/sub/PKG-INFO", "nope")] {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append_data(&mut h, name, body.as_bytes()).unwrap();
        }
        std::fs::write(&path, builder.into_inner().unwrap().finish().unwrap()).unwrap();
        assert!(read_pkg_info(&path).unwrap().contains("Name: demo"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
