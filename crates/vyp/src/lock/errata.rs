//! Errata feeds: TOML files of corrections to already-published releases.
//!
//! ```toml
//! errata-version = "1"
//!
//! [[errata]]
//! package = "green"
//! versions = "<4"
//! requires-python = "<3.12"
//! reason = "green 3.x breaks on Python 3.12"
//!
//! [[errata]]
//! package = "foo"
//! versions = "==1.2.0"
//! add-requires = ["bar<2"]
//!
//! [[errata]]
//! package = "baz"
//! versions = ">=3.1,<3.1.4"
//! exclude = true
//! ```
//!
//! A feed is a URL or a path relative to the project. Remote feeds are cached
//! so a lock still works offline; the digest of the content used is recorded
//! in the lock file.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use vyp_api::{MarkerEnvironment, Requirement, VypVersion};
use vyp_core::resolver::requirements_to_range;
use vyp_core::{ErrataFeedDigest, Erratum};

use crate::cache::wheel_cache::cache_base_dir;

const SUPPORTED_VERSION: &str = "1";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFeed {
    #[serde(rename = "errata-version")]
    version: String,
    #[serde(default)]
    errata: Vec<RawErratum>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawErratum {
    package: String,
    #[serde(default)]
    versions: String,
    #[serde(default, rename = "requires-python")]
    requires_python: Option<String>,
    #[serde(default, rename = "add-requires")]
    add_requires: Vec<String>,
    #[serde(default)]
    exclude: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// A parsed erratum, not yet evaluated for a target environment.
#[derive(Debug, Clone)]
pub struct FeedErratum {
    raw: RawErratum,
    source: String,
}

/// The errata of every configured feed.
#[derive(Debug, Clone, Default)]
pub struct LoadedErrata {
    pub errata: Vec<FeedErratum>,
    pub feeds: Vec<ErrataFeedDigest>,
}

impl LoadedErrata {
    /// Evaluate `Requires-Python` and requirement markers for one target
    /// environment, producing the errata the resolver applies there.
    pub fn for_environment(&self, env: &MarkerEnvironment) -> miette::Result<Vec<Erratum>> {
        let python: Option<VypVersion> = env.python_full_version.parse().ok();
        let mut out = Vec::new();
        for item in &self.errata {
            let raw = &item.raw;
            let versions = parse_spec(&raw.package, &raw.versions, &item.source)?;
            let mut erratum = Erratum::new(&raw.package, versions, item.source.clone());
            erratum.versions_spec = raw.versions.clone();
            erratum.reason = raw.reason.clone();
            erratum.exclude = raw.exclude;

            if let (Some(spec), Some(python)) = (&raw.requires_python, &python) {
                if !parse_spec("python", spec, &item.source)?.contains(python) {
                    erratum.exclude = true;
                }
            }

            for req_str in &raw.add_requires {
                let req: Requirement = req_str.parse().map_err(|e| {
                    miette::miette!(
                        "Invalid add-requires '{}' in {}: {}",
                        req_str,
                        item.source,
                        e
                    )
                })?;
                if req.marker.as_ref().is_none_or(|m| m.evaluate(env, &[])) {
                    erratum.requires.push(req);
                }
            }

            if erratum.exclude || !erratum.requires.is_empty() {
                out.push(erratum);
            }
        }
        Ok(out)
    }
}

fn parse_spec(package: &str, spec: &str, source: &str) -> miette::Result<vyp_core::resolver::VS> {
    let req: Requirement = format!("{} {}", package, spec).parse().map_err(|e| {
        miette::miette!(
            "Invalid version specifier '{}' for {} in {}: {}",
            spec,
            package,
            source,
            e
        )
    })?;
    Ok(requirements_to_range(&req.constraints))
}

/// Load every feed. Remote feeds fall back to their cached copy when the
/// network is unavailable.
pub fn load_feeds(
    feeds: &[String],
    project_dir: Option<&Path>,
    client: &reqwest::Client,
) -> miette::Result<LoadedErrata> {
    let mut loaded = LoadedErrata::default();
    for source in feeds {
        let content = if is_url(source) {
            fetch_remote(source, client)?
        } else {
            let path = project_dir.map_or_else(|| PathBuf::from(source), |d| d.join(source));
            std::fs::read_to_string(&path).map_err(|e| {
                miette::miette!("Failed to read errata feed {}: {}", path.display(), e)
            })?
        };
        let feed = parse_feed(&content, source)?;
        loaded.feeds.push(ErrataFeedDigest {
            source: source.clone(),
            sha256: hex_sha256(content.as_bytes()),
        });
        loaded
            .errata
            .extend(feed.errata.into_iter().map(|raw| FeedErratum {
                raw,
                source: source.clone(),
            }));
    }
    Ok(loaded)
}

fn parse_feed(content: &str, source: &str) -> miette::Result<RawFeed> {
    let feed: RawFeed = toml::from_str(content)
        .map_err(|e| miette::miette!("Failed to parse errata feed {}: {}", source, e))?;
    if feed.version != SUPPORTED_VERSION {
        return Err(miette::miette!(
            "Errata feed {} has errata-version {}; this vyp supports {}",
            source,
            feed.version,
            SUPPORTED_VERSION
        ));
    }
    for e in &feed.errata {
        if !e.exclude && e.requires_python.is_none() && e.add_requires.is_empty() {
            return Err(miette::miette!(
                "Erratum for {} in {} has no effect: set exclude, requires-python or add-requires",
                e.package,
                source
            ));
        }
    }
    Ok(feed)
}

fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn cache_path(url: &str) -> PathBuf {
    cache_base_dir()
        .join("errata")
        .join(format!("{}.toml", hex_sha256(url.as_bytes())))
}

fn fetch_remote(url: &str, client: &reqwest::Client) -> miette::Result<String> {
    let cached = cache_path(url);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| miette::miette!("Failed to start runtime: {}", e))?;
    let fetched: Result<String, String> = rt.block_on(async {
        let resp = vyp_index::auth::apply_auth(client.get(url), url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        resp.text().await.map_err(|e| e.to_string())
    });

    match fetched {
        Ok(body) => {
            // Only cache content that parses, so a broken response never
            // replaces a good offline copy.
            if parse_feed(&body, url).is_ok() {
                if let Some(dir) = cached.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&cached, &body);
            }
            Ok(body)
        }
        Err(err) => match std::fs::read_to_string(&cached) {
            Ok(body) => {
                eprintln!(
                    "warning: could not fetch errata feed {} ({}); using cached copy",
                    url, err
                );
                Ok(body)
            }
            Err(_) => Err(miette::miette!(
                "Failed to fetch errata feed {}: {}",
                url,
                err
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"
errata-version = "1"

[[errata]]
package = "green"
versions = "<4"
requires-python = "<3.12"
reason = "breaks on 3.12"

[[errata]]
package = "foo"
versions = "==1.2.0"
add-requires = ["bar<2", "winonly; sys_platform == 'win32'"]
"#;

    fn loaded() -> LoadedErrata {
        let feed = parse_feed(FEED, "feed.toml").unwrap();
        LoadedErrata {
            errata: feed
                .errata
                .into_iter()
                .map(|raw| FeedErratum {
                    raw,
                    source: "feed.toml".into(),
                })
                .collect(),
            feeds: Vec::new(),
        }
    }

    #[test]
    fn requires_python_excludes_only_on_affected_interpreters() {
        let errata = loaded();

        let py312 = errata
            .for_environment(&MarkerEnvironment::for_python_version(3, 12))
            .unwrap();
        let green = py312.iter().find(|e| e.package == "green").unwrap();
        assert!(green.exclude);
        assert!(green.versions.contains(&VypVersion::from_parts(3, 9, 0)));
        assert!(!green.versions.contains(&VypVersion::from_parts(4, 0, 0)));

        let py311 = errata
            .for_environment(&MarkerEnvironment::for_python_version(3, 11))
            .unwrap();
        assert!(py311.iter().all(|e| e.package != "green"));
    }

    #[test]
    fn add_requires_markers_are_evaluated() {
        let mut env = MarkerEnvironment::for_python_version(3, 12);
        env.sys_platform = "linux".into();
        let errata = loaded().for_environment(&env).unwrap();
        let foo = errata.iter().find(|e| e.package == "foo").unwrap();
        let names: Vec<&str> = foo.requires.iter().map(|r| r.package.name()).collect();
        assert_eq!(names, vec!["bar"]);
    }

    #[test]
    fn rejects_unknown_fields_and_no_op_entries() {
        let typo = "errata-version = \"1\"\n[[errata]]\npackage = \"x\"\nexlude = true\n";
        assert!(parse_feed(typo, "t").is_err());
        let noop = "errata-version = \"1\"\n[[errata]]\npackage = \"x\"\n";
        assert!(parse_feed(noop, "t").is_err());
        let future = "errata-version = \"2\"\n";
        assert!(parse_feed(future, "t").is_err());
    }
}
