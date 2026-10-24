//! Git dependencies: a bare clone per repository URL, and a checkout per
//! commit, both under the cache. Uses the `git` command, so credentials,
//! SSH config and URL rewrites work as they do for the user.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cache::wheel_cache::cache_base_dir;
use crate::python::hex_sha256;

/// A parsed `git+URL[@rev][#subdirectory=path]` reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    pub url: String,
    pub rev: Option<String>,
    pub subdirectory: Option<String>,
}

impl GitRef {
    /// Parse a PEP 440 direct reference such as
    /// `git+https://github.com/org/repo@v1.2#subdirectory=pkg`.
    pub fn parse(reference: &str) -> Option<Self> {
        let rest = reference.trim().strip_prefix("git+")?;
        let (rest, fragment) = match rest.split_once('#') {
            Some((r, f)) => (r, Some(f)),
            None => (rest, None),
        };
        let subdirectory = fragment.and_then(|f| {
            f.split('&')
                .find_map(|kv| kv.strip_prefix("subdirectory="))
                .map(String::from)
        });
        // The revision follows the last `@` in the path, not one in
        // `ssh://git@host`.
        let path_start = rest.find("://").map(|i| i + 3).unwrap_or(0);
        let path_start = rest[path_start..]
            .find('/')
            .map(|i| path_start + i)
            .unwrap_or(rest.len());
        let (url, rev) = match rest[path_start..].rfind('@') {
            Some(i) => (
                &rest[..path_start + i],
                Some(rest[path_start + i + 1..].to_string()),
            ),
            None => (rest, None),
        };
        Some(Self {
            url: url.to_string(),
            rev: rev.filter(|r| !r.is_empty()),
            subdirectory,
        })
    }
}

fn git(args: &[&str], cwd: Option<&Path>) -> miette::Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).env("GIT_TERMINAL_PROMPT", "0");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .map_err(|e| miette::miette!("Failed to run git (is it installed?): {}", e))?;
    if !out.status.success() {
        return Err(miette::miette!(
            "git {} failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn db_dir(url: &str) -> PathBuf {
    cache_base_dir()
        .join("git")
        .join("db")
        .join(&hex_sha256(url.as_bytes())[..24])
}

/// Make sure the bare clone exists and has `rev` (fetching if needed);
/// return the full commit id `rev` names.
pub fn resolve_commit(url: &str, rev: Option<&str>) -> miette::Result<String> {
    let db = db_dir(url);
    let rev = rev.unwrap_or("HEAD");
    if !db.join("HEAD").exists() {
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).map_err(|e| miette::miette!("{}", e))?;
        }
        git(
            &["clone", "--bare", "--quiet", url, &db.to_string_lossy()],
            None,
        )?;
    }
    let lookup = |r: &str| {
        git(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{}^{{commit}}", r),
            ],
            Some(&db),
        )
    };
    // A full commit id already present needs no network.
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(commit) = lookup(rev) {
            return Ok(commit);
        }
    }
    git(
        &[
            "fetch",
            "--quiet",
            "--force",
            "--tags",
            "origin",
            "+refs/heads/*:refs/heads/*",
        ],
        Some(&db),
    )?;
    if let Ok(commit) = lookup(rev) {
        return Ok(commit);
    }
    // Refs outside heads/tags (e.g. a pull request ref) or an unadvertised commit.
    git(&["fetch", "--quiet", "origin", rev], Some(&db))?;
    lookup(rev).or_else(|_| lookup("FETCH_HEAD"))
}

/// A working tree at `commit`, created on first use.
pub fn checkout(url: &str, commit: &str) -> miette::Result<PathBuf> {
    let db = db_dir(url);
    if !db.join("HEAD").exists() {
        resolve_commit(url, Some(commit))?;
    }
    let dir = cache_base_dir()
        .join("git")
        .join("checkouts")
        .join(&hex_sha256(url.as_bytes())[..24])
        .join(commit);
    if dir.join(".vyp-complete").exists() {
        return Ok(dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| miette::miette!("{}", e))?;
    }
    git(
        &[
            "clone",
            "--quiet",
            "--shared",
            "--no-checkout",
            &db.to_string_lossy(),
            &dir.to_string_lossy(),
        ],
        None,
    )?;
    git(&["checkout", "--quiet", "--detach", commit], Some(&dir))?;
    // Submodules point at their real remotes, not the bare cache.
    git(&["remote", "set-url", "origin", url], Some(&dir))?;
    if dir.join(".gitmodules").exists() {
        git(
            &["submodule", "update", "--init", "--recursive", "--quiet"],
            Some(&dir),
        )?;
    }
    std::fs::write(dir.join(".vyp-complete"), b"").map_err(|e| miette::miette!("{}", e))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_references() {
        assert_eq!(
            GitRef::parse("git+https://github.com/org/repo@v1.2#subdirectory=pkg/sub"),
            Some(GitRef {
                url: "https://github.com/org/repo".into(),
                rev: Some("v1.2".into()),
                subdirectory: Some("pkg/sub".into()),
            })
        );
        assert_eq!(
            GitRef::parse("git+ssh://git@github.com/org/repo.git"),
            Some(GitRef {
                url: "ssh://git@github.com/org/repo.git".into(),
                rev: None,
                subdirectory: None
            })
        );
        assert_eq!(
            GitRef::parse("git+ssh://git@github.com/org/repo.git@main"),
            Some(GitRef {
                url: "ssh://git@github.com/org/repo.git".into(),
                rev: Some("main".into()),
                subdirectory: None
            })
        );
        assert_eq!(GitRef::parse("https://example.com/x.whl"), None);
    }

    #[test]
    fn resolves_and_checks_out_a_local_repository() {
        let root = std::env::temp_dir().join(format!("vyp-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| git(args, Some(&repo)).unwrap();
        run(&["init", "--quiet", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one").unwrap();
        run(&["add", "."]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-m",
            "one",
        ]);
        run(&["tag", "v1"]);
        std::fs::write(repo.join("a.txt"), "two").unwrap();
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--quiet",
            "-am",
            "two",
        ]);

        let url = format!("file://{}", repo.display());
        let v1 = resolve_commit(&url, Some("v1")).unwrap();
        let head = resolve_commit(&url, None).unwrap();
        assert_ne!(v1, head);
        let tree = checkout(&url, &v1).unwrap();
        assert_eq!(std::fs::read_to_string(tree.join("a.txt")).unwrap(), "one");
        let _ = std::fs::remove_dir_all(&root);
    }
}
