//! Index authentication via `~/.netrc` (or `$NETRC`).
//!
//! Private package indexes commonly require HTTP Basic auth. We resolve
//! credentials per-host from netrc once per process and attach them to index
//! requests, matching pip's default behaviour.

use std::collections::HashMap;
use std::sync::OnceLock;

/// host -> (login, password)
type NetrcMap = HashMap<String, (String, String)>;

fn netrc() -> &'static NetrcMap {
    static NETRC: OnceLock<NetrcMap> = OnceLock::new();
    NETRC.get_or_init(load_netrc)
}

fn netrc_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("NETRC") {
        if !p.is_empty() {
            return Some(std::path::PathBuf::from(p));
        }
    }
    let home = std::env::var("HOME").ok()?;
    let candidate = std::path::Path::new(&home).join(".netrc");
    if candidate.exists() {
        return Some(candidate);
    }
    // Windows convention.
    let alt = std::path::Path::new(&home).join("_netrc");
    if alt.exists() {
        return Some(alt);
    }
    None
}

fn load_netrc() -> NetrcMap {
    let mut map = NetrcMap::new();
    let Some(path) = netrc_path() else { return map };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return map;
    };
    parse_netrc(&content, &mut map);
    map
}

/// Parse netrc `machine`/`login`/`password`/`default` entries. The format is a
/// flat whitespace-separated token stream; tokens may span lines.
fn parse_netrc(content: &str, map: &mut NetrcMap) {
    let mut tokens = content.split_whitespace().peekable();
    let mut current_host: Option<String> = None;
    let mut login: Option<String> = None;
    let mut password: Option<String> = None;

    fn flush(
        host: &mut Option<String>,
        login: &mut Option<String>,
        password: &mut Option<String>,
        map: &mut NetrcMap,
    ) {
        if let (Some(h), Some(l), Some(p)) = (host.take(), login.take(), password.take()) {
            map.insert(h, (l, p));
        } else {
            host.take();
            login.take();
            password.take();
        }
    }

    while let Some(tok) = tokens.next() {
        match tok {
            "machine" => {
                flush(&mut current_host, &mut login, &mut password, map);
                current_host = tokens.next().map(|s| s.to_string());
            }
            "default" => {
                flush(&mut current_host, &mut login, &mut password, map);
                current_host = Some("default".to_string());
            }
            "login" => login = tokens.next().map(|s| s.to_string()),
            "password" => password = tokens.next().map(|s| s.to_string()),
            "account" | "macdef" => {
                let _ = tokens.next();
            }
            _ => {}
        }
    }
    flush(&mut current_host, &mut login, &mut password, map);
}

/// Look up credentials for a URL's host, falling back to a `default` entry.
pub fn credentials_for(url: &str) -> Option<(String, String)> {
    let host = host_of(url)?;
    let m = netrc();
    m.get(&host).or_else(|| m.get("default")).cloned()
}

fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let authority = after_scheme.split('/').next()?;
    // Strip any userinfo and port.
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Attach Basic auth to a request if netrc has credentials for its URL.
pub fn apply_auth(req: reqwest::RequestBuilder, url: &str) -> reqwest::RequestBuilder {
    match credentials_for(url) {
        Some((user, pass)) => req.basic_auth(user, Some(pass)),
        None => req,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_netrc() {
        let mut map = NetrcMap::new();
        parse_netrc(
            "machine example.com login alice password s3cret\n\
             machine pypi.internal\n  login bob\n  password hunter2\n",
            &mut map,
        );
        assert_eq!(
            map.get("example.com"),
            Some(&("alice".to_string(), "s3cret".to_string()))
        );
        assert_eq!(
            map.get("pypi.internal"),
            Some(&("bob".to_string(), "hunter2".to_string()))
        );
    }

    #[test]
    fn default_entry() {
        let mut map = NetrcMap::new();
        parse_netrc("default login u password p", &mut map);
        assert_eq!(
            map.get("default"),
            Some(&("u".to_string(), "p".to_string()))
        );
    }

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("https://pypi.org/simple"),
            Some("pypi.org".to_string())
        );
        assert_eq!(
            host_of("https://user:pw@nexus.corp:8443/repo"),
            Some("nexus.corp".to_string())
        );
    }
}
