use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::cache::venv::VenvLayout;

/// One installed-file record for the `.dist-info/RECORD` manifest.
struct RecordEntry {
    /// Path of the installed file relative to site-packages (forward slashes),
    /// e.g. `requests/__init__.py` or `../../../bin/normalizer`.
    rel: String,
    /// `sha256=<urlsafe-b64-nopad>` digest, or empty for the RECORD file itself.
    hash: String,
    size: u64,
}

/// Install a pre-extracted wheel archive into a virtual environment.
///
/// This goes beyond a verbatim file copy: it relocates the wheel's
/// `*.data/{scripts,data,headers,purelib,platlib}` payloads to their scheme
/// directories, generates `console_scripts`/`gui_scripts` launchers from
/// `entry_points.txt`, rewrites `#!python` shebangs in bundled scripts, and
/// writes the `INSTALLER` and `RECORD` files so the install is introspectable
/// and uninstallable.
///
/// `file_list`, when provided, is the archive's relative file list (skips a
/// filesystem walk). `assume_fresh` skips per-file existence checks when the
/// target site-packages started empty. `requested` marks the package as a
/// direct (top-level) request, adding a `REQUESTED` marker per PEP 376.
pub fn install_wheel(
    archive_dir: &Path,
    layout: &VenvLayout,
    file_list: Option<&[PathBuf]>,
    assume_fresh: bool,
    requested: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Resolve the archive's relative file list once.
    let files: Vec<PathBuf> = match file_list {
        Some(f) => f.to_vec(),
        None => walkdir::WalkDir::new(archive_dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                e.path()
                    .strip_prefix(archive_dir)
                    .ok()
                    .map(|p| p.to_path_buf())
            })
            .filter(|p| !p.as_os_str().is_empty())
            .collect(),
    };

    // Identify the top-level `*.data` and `*.dist-info` directory names.
    let data_dir = top_level_dir_with_suffix(&files, ".data");
    let dist_info = top_level_dir_with_suffix(&files, ".dist-info");

    // Reuse the hashes the wheel already published in its own RECORD instead of
    // re-hashing every installed file — re-hashing would read every byte and
    // defeat the reflink/hardlink fast path. Only relocated and freshly
    // generated files (a small handful) are hashed below.
    let wheel_record: std::collections::HashMap<String, (String, u64)> = dist_info
        .as_ref()
        .map(|di| parse_wheel_record(&archive_dir.join(di).join("RECORD")))
        .unwrap_or_default();

    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    let mut records: Vec<RecordEntry> = Vec::new();

    // Whole package directories the venv doesn't have yet are cloned in one
    // call each; only their RECORD entries are needed below.
    let cloned = clone_top_level_dirs(archive_dir, &files, layout, data_dir.as_deref());

    for rel in &files {
        if !cloned.is_empty() {
            if let Some(top) = rel.components().next() {
                if cloned.contains(top.as_os_str()) {
                    let rel_key = rel.to_string_lossy().replace('\\', "/");
                    let is_wheel_record = dist_info
                        .as_ref()
                        .is_some_and(|di| rel_key == format!("{}/RECORD", di.to_string_lossy()));
                    if is_wheel_record {
                        continue;
                    }
                    if let Some((hash, size)) = wheel_record.get(&rel_key) {
                        records.push(RecordEntry {
                            rel: rel_key,
                            hash: hash.clone(),
                            size: *size,
                        });
                    } else if let Some(entry) =
                        record_entry(layout, &layout.site_packages.join(rel))?
                    {
                        records.push(entry);
                    }
                    continue;
                }
            }
        }

        let source = archive_dir.join(rel);
        if source.is_dir() {
            continue;
        }

        // Route the file: a `*.data/<scheme>/...` payload is relocated, all
        // other files land in site-packages.
        let (target, is_script, from_data) =
            match data_dir.as_deref().and_then(|d| rel.strip_prefix(d).ok()) {
                Some(inner) => {
                    let mut comps = inner.components();
                    let scheme = comps
                        .next()
                        .map(|c| c.as_os_str().to_string_lossy().to_string())
                        .unwrap_or_default();
                    let sub: PathBuf = comps.as_path().to_path_buf();
                    if sub.as_os_str().is_empty() {
                        continue;
                    }
                    let dest_dir = layout.data_scheme_target(&scheme);
                    (dest_dir.join(&sub), scheme == "scripts", true)
                }
                None => (layout.site_packages.join(rel), false, false),
            };

        ensure_parent(&target, &mut created_dirs)?;

        if is_script {
            install_script_file(&source, &target, layout)?;
        } else {
            place_file(&source, &target, assume_fresh)?;
        }

        // The wheel's own RECORD is regenerated below; don't carry it over.
        let rel_key = rel.to_string_lossy().replace('\\', "/");
        if dist_info
            .as_ref()
            .is_some_and(|di| rel_key == format!("{}/RECORD", di.to_string_lossy()))
        {
            continue;
        }

        if !from_data {
            // Verbatim wheel file: reuse the published hash/size (no I/O).
            if let Some((hash, size)) = wheel_record.get(&rel_key) {
                records.push(RecordEntry {
                    rel: rel_key,
                    hash: hash.clone(),
                    size: *size,
                });
                continue;
            }
        }
        // Relocated payload, rewritten script, or a file the wheel RECORD
        // didn't list — hash it directly.
        if let Some(entry) = record_entry(layout, &target)? {
            records.push(entry);
        }
    }

    // Generate entry-point launchers and capture them in RECORD.
    if let Some(di) = &dist_info {
        let ep_path = archive_dir.join(di).join("entry_points.txt");
        if ep_path.exists() {
            let content = std::fs::read_to_string(&ep_path)?;
            for script in parse_entry_point_scripts(&content) {
                let target = generate_launcher(layout, &script)?;
                if let Some(entry) = record_entry(layout, &target)? {
                    records.push(entry);
                }
            }
        }

        // Write INSTALLER, REQUESTED, then RECORD (RECORD lists itself last).
        let dist_info_dir = layout.site_packages.join(di);
        std::fs::create_dir_all(&dist_info_dir)?;

        let installer = dist_info_dir.join("INSTALLER");
        std::fs::write(&installer, b"vyp\n")?;
        if let Some(e) = record_entry(layout, &installer)? {
            records.push(e);
        }

        if requested {
            let req = dist_info_dir.join("REQUESTED");
            std::fs::write(&req, b"")?;
            if let Some(e) = record_entry(layout, &req)? {
                records.push(e);
            }
        }

        write_record(layout, di, &records)?;
    }

    Ok(())
}

/// Backwards-compatible thin copy used where venv layout is unavailable
/// (kept for the dry-run / legacy path): copies files verbatim into
/// site-packages without scheme relocation.
#[allow(dead_code)]
pub fn install_from_archive(
    archive_dir: &Path,
    site_packages: &Path,
    file_list: Option<&[PathBuf]>,
    assume_fresh: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut created_dirs: HashSet<PathBuf> = HashSet::new();
    let walk_owned;
    let files: &[PathBuf] = match file_list {
        Some(f) => f,
        None => {
            walk_owned = walkdir::WalkDir::new(archive_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    e.path()
                        .strip_prefix(archive_dir)
                        .ok()
                        .map(|p| p.to_path_buf())
                })
                .filter(|p| !p.as_os_str().is_empty())
                .collect::<Vec<_>>();
            &walk_owned
        }
    };
    for rel in files {
        let source = archive_dir.join(rel);
        if source.is_dir() {
            continue;
        }
        let target = site_packages.join(rel);
        ensure_parent(&target, &mut created_dirs)?;
        place_file(&source, &target, assume_fresh)?;
    }
    Ok(())
}

/// Clone each top-level directory of the wheel that installs into
/// site-packages and doesn't exist there yet, with one `clonefile(2)` per
/// directory tree instead of one per file. Each clone goes to a temporary
/// name and is renamed into place, so a concurrent install of another
/// package sharing the directory (a namespace package) falls back to the
/// per-file merge instead of racing. Returns the names that were cloned.
#[cfg(target_os = "macos")]
fn clone_top_level_dirs(
    archive_dir: &Path,
    files: &[PathBuf],
    layout: &VenvLayout,
    data_dir: Option<&Path>,
) -> HashSet<std::ffi::OsString> {
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    const CLONE_NOFOLLOW: u32 = 0x0001;

    let mut cloned = HashSet::new();
    let tops: HashSet<&std::ffi::OsStr> = files
        .iter()
        .filter(|f| f.components().count() > 1)
        .filter_map(|f| f.components().next().map(|c| c.as_os_str()))
        .filter(|top| data_dir.is_none_or(|d| d.as_os_str() != *top))
        .collect();
    if tops.is_empty() || std::fs::create_dir_all(&layout.site_packages).is_err() {
        return cloned;
    }
    for top in tops {
        let target = layout.site_packages.join(top);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        let tmp = layout.site_packages.join(format!(
            ".vyp-clone-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let (Ok(src), Ok(dst)) = (
            std::ffi::CString::new(archive_dir.join(top).as_os_str().as_bytes()),
            std::ffi::CString::new(tmp.as_os_str().as_bytes()),
        ) else {
            continue;
        };
        // SAFETY: both arguments are valid NUL-terminated paths.
        if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), CLONE_NOFOLLOW) } != 0 {
            // Different volume, not APFS, or similar: install file by file.
            let _ = std::fs::remove_dir_all(&tmp);
            continue;
        }
        if std::fs::rename(&tmp, &target).is_ok() {
            cloned.insert(top.to_os_string());
        } else {
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
    cloned
}

#[cfg(not(target_os = "macos"))]
fn clone_top_level_dirs(
    _archive_dir: &Path,
    _files: &[PathBuf],
    _layout: &VenvLayout,
    _data_dir: Option<&Path>,
) -> HashSet<std::ffi::OsString> {
    HashSet::new()
}

fn ensure_parent(
    target: &Path,
    created_dirs: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(parent) = target.parent() {
        if created_dirs.insert(parent.to_path_buf()) {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Place a file via reflink (CoW) → hardlink → copy fallback.
fn place_file(
    source: &Path,
    target: &Path,
    assume_fresh: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !assume_fresh && target.exists() {
        std::fs::remove_file(target)?;
    }
    if reflink_copy::reflink(source, target).is_ok() {
        return Ok(());
    }
    if std::fs::hard_link(source, target).is_ok() {
        return Ok(());
    }
    std::fs::copy(source, target)?;
    Ok(())
}

/// Install a bundled `*.data/scripts/` file: copy it (not hardlink, since we
/// may rewrite the shebang), fix a `#!python` shebang, and set the exec bit.
fn install_script_file(
    source: &Path,
    target: &Path,
    layout: &VenvLayout,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut bytes = Vec::new();
    std::fs::File::open(source)?.read_to_end(&mut bytes)?;

    // Rewrite a leading `#!...python...` shebang to the venv interpreter.
    if bytes.starts_with(b"#!") {
        if let Some(nl) = bytes.iter().position(|&b| b == b'\n') {
            let first = String::from_utf8_lossy(&bytes[..nl]);
            if first.contains("python") {
                let mut rewritten = layout.script_header().into_bytes();
                rewritten.extend_from_slice(&bytes[nl + 1..]);
                bytes = rewritten;
            }
        }
    }

    if target.exists() {
        std::fs::remove_file(target)?;
    }
    std::fs::write(target, &bytes)?;
    set_executable(target)?;
    Ok(())
}

/// A console/gui script entry point parsed from `entry_points.txt`.
struct EntryScript {
    name: String,
    module: String,
    attr: String,
    gui: bool,
}

/// Parse `[console_scripts]` / `[gui_scripts]` sections from `entry_points.txt`.
fn parse_entry_point_scripts(content: &str) -> Vec<EntryScript> {
    let mut out = Vec::new();
    let mut section = String::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(sec) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = sec.trim().to_string();
            continue;
        }
        let gui = match section.as_str() {
            "console_scripts" => false,
            "gui_scripts" => true,
            _ => continue,
        };
        let Some((name, target)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        // Strip any `[extras]` suffix from the target.
        let target = target.split('[').next().unwrap_or("").trim();
        let Some((module, attr)) = target.split_once(':') else {
            continue;
        };
        let (module, attr) = (module.trim(), attr.trim());
        if name.is_empty() || module.is_empty() || attr.is_empty() {
            continue;
        }
        out.push(EntryScript {
            name: name.to_string(),
            module: module.to_string(),
            attr: attr.to_string(),
            gui,
        });
    }
    out
}

/// Generate a launcher for a console/gui script and return its path.
fn generate_launcher(
    layout: &VenvLayout,
    script: &EntryScript,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    std::fs::create_dir_all(&layout.scripts)?;
    let import_root = script.attr.split('.').next().unwrap_or(&script.attr);

    if cfg!(windows) {
        // Best-effort Windows launcher: a .bat that invokes the venv python.
        let target = layout.scripts.join(format!("{}.bat", script.name));
        let body = format!(
            "@echo off\r\n\"{}\" -c \"import sys; from {} import {}; sys.exit({}())\" %*\r\n",
            layout.python_exe.display(),
            script.module,
            import_root,
            script.attr,
        );
        std::fs::write(&target, body)?;
        return Ok(target);
    }

    let target = layout.scripts.join(&script.name);
    let _ = script.gui; // POSIX launcher is identical for console/gui.
    let body = format!(
        "{header}\
         # -*- coding: utf-8 -*-\n\
         # Generated by vyp.\n\
         import re\n\
         import sys\n\
         from {module} import {import_root}\n\
         if __name__ == \"__main__\":\n\
         \x20   sys.argv[0] = re.sub(r\"(-script\\.pyw?|\\.exe)?$\", \"\", sys.argv[0])\n\
         \x20   sys.exit({attr}())\n",
        header = layout.script_header(),
        module = script.module,
        import_root = import_root,
        attr = script.attr,
    );
    std::fs::write(&target, body)?;
    set_executable(&target)?;
    Ok(target)
}

#[cfg(unix)]
fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Build a RECORD entry for an installed file (path relative to site-packages,
/// sha256 digest, size).
fn record_entry(
    layout: &VenvLayout,
    target: &Path,
) -> Result<Option<RecordEntry>, Box<dyn std::error::Error + Send + Sync>> {
    let meta = match std::fs::metadata(target) {
        Ok(m) if m.is_file() => m,
        _ => return Ok(None),
    };
    let mut hasher = Sha256::new();
    let mut f = std::fs::File::open(target)?;
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize();
    let hash = format!("sha256={}", base64_urlsafe_nopad(&digest));
    let rel = relative_to(&layout.site_packages, target);
    Ok(Some(RecordEntry {
        rel,
        hash,
        size: meta.len(),
    }))
}

/// Parse a wheel's bundled `RECORD` into `path -> (hash, size)`, so installed
/// files can reuse the published digests instead of being re-hashed. Entries
/// with no hash or size (e.g. the wheel's own RECORD line) are skipped.
fn parse_wheel_record(record_path: &Path) -> std::collections::HashMap<String, (String, u64)> {
    let mut map = std::collections::HashMap::new();
    let Ok(content) = std::fs::read_to_string(record_path) else {
        return map;
    };
    for line in content.lines() {
        // RECORD is CSV: path,hash,size. Paths may be quoted if they contain
        // commas; wheel paths almost never are, so split from the right on the
        // last two commas to isolate hash and size.
        let line = line.trim_end_matches(['\r', '\n']);
        let Some((rest, size)) = line.rsplit_once(',') else {
            continue;
        };
        let Some((path, hash)) = rest.rsplit_once(',') else {
            continue;
        };
        if hash.is_empty() || size.is_empty() {
            continue;
        }
        let Ok(size) = size.parse::<u64>() else {
            continue;
        };
        let path = path.trim_matches('"').to_string();
        map.insert(path, (hash.to_string(), size));
    }
    map
}

/// Write the `.dist-info/RECORD` manifest (RECORD lists itself with no hash).
fn write_record(
    layout: &VenvLayout,
    dist_info: &Path,
    records: &[RecordEntry],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let record_path = layout.site_packages.join(dist_info).join("RECORD");
    let record_rel = relative_to(&layout.site_packages, &record_path);

    let mut out = String::new();
    for r in records.iter() {
        out.push_str(&format!("{},{},{}\n", csv_quote(&r.rel), r.hash, r.size));
    }
    out.push_str(&format!("{},,\n", csv_quote(&record_rel)));
    std::fs::write(&record_path, out)?;
    Ok(())
}

/// Compute `target` relative to `base`, emitting `../` segments as needed and
/// using forward slashes (RECORD/uninstall path format).
pub fn relative_to(base: &Path, target: &Path) -> String {
    let base_c: Vec<_> = base.components().collect();
    let targ_c: Vec<_> = target.components().collect();
    let mut i = 0;
    while i < base_c.len() && i < targ_c.len() && base_c[i] == targ_c[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in i..base_c.len() {
        parts.push("..".to_string());
    }
    for c in &targ_c[i..] {
        parts.push(c.as_os_str().to_string_lossy().to_string());
    }
    parts.join("/")
}

fn csv_quote(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Verify that an extracted wheel's `*.dist-info/METADATA` declares the
/// expected distribution name and version. Used as a defense-in-depth check
/// when the lock has no integrity hash to pin (a matching sha256 is a stronger
/// guarantee and makes this redundant). Normalizes names before comparing.
pub fn verify_wheel_identity(
    archive_dir: &Path,
    expected_name: &str,
    expected_version: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata_path = walkdir::WalkDir::new(archive_dir)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.path().to_string_lossy().ends_with(".dist-info/METADATA"));
    let Some(entry) = metadata_path else {
        // No METADATA found (unusual); don't block the install on it.
        return Ok(());
    };

    let content = std::fs::read_to_string(entry.path())?;
    let mut got_name = None;
    let mut got_version = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("Name:") {
            got_name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("Version:") {
            got_version = Some(v.trim().to_string());
        }
        if line.is_empty() {
            break; // headers end at the first blank line
        }
    }

    if let Some(name) = &got_name {
        let norm = |s: &str| s.to_lowercase().replace(['-', '.', '_'], "_");
        if norm(name) != norm(expected_name) {
            return Err(format!(
                "wheel identity mismatch: expected name '{}', wheel declares '{}'",
                expected_name, name
            )
            .into());
        }
    }
    if let Some(version) = &got_version {
        if version != expected_version {
            return Err(format!(
                "wheel identity mismatch for {}: expected version '{}', wheel declares '{}'",
                expected_name, expected_version, version
            )
            .into());
        }
    }
    Ok(())
}

fn top_level_dir_with_suffix(files: &[PathBuf], suffix: &str) -> Option<PathBuf> {
    for f in files {
        if let Some(first) = f.components().next() {
            let name = first.as_os_str().to_string_lossy();
            if name.ends_with(suffix) {
                return Some(PathBuf::from(first.as_os_str()));
            }
        }
    }
    None
}

/// URL-safe base64 without padding (the RECORD hash encoding per PEP 376).
fn base64_urlsafe_nopad(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_console_scripts() {
        let content = "\
[console_scripts]
black = black:patched_main
blackd = blackd:main [d]

[gui_scripts]
napari = napari.__main__:main

[other]
ignored = x:y
";
        let scripts = parse_entry_point_scripts(content);
        assert_eq!(scripts.len(), 3);
        assert_eq!(scripts[0].name, "black");
        assert_eq!(scripts[0].module, "black");
        assert_eq!(scripts[0].attr, "patched_main");
        assert!(!scripts[0].gui);
        assert_eq!(scripts[1].name, "blackd");
        assert_eq!(scripts[1].attr, "main");
        assert!(scripts[2].gui);
        assert_eq!(scripts[2].module, "napari.__main__");
    }

    #[test]
    fn base64_matches_known_vector() {
        // sha256("") urlsafe-b64 nopad, as pip writes for empty files.
        let digest = Sha256::digest(b"");
        let enc = base64_urlsafe_nopad(&digest);
        assert_eq!(enc, "47DEQpj8HBSa-_TImW-5JCeuQeRkm5NMpJWZG3hSuFU");
    }

    #[test]
    fn relative_path_same_dir() {
        let base = Path::new("/venv/lib/python3.11/site-packages");
        let target = Path::new("/venv/lib/python3.11/site-packages/requests/__init__.py");
        assert_eq!(relative_to(base, target), "requests/__init__.py");
    }

    #[test]
    fn relative_path_to_bin() {
        let base = Path::new("/venv/lib/python3.11/site-packages");
        let target = Path::new("/venv/bin/black");
        assert_eq!(relative_to(base, target), "../../../bin/black");
    }

    fn fake_wheel(root: &Path, name: &str, files: &[&str]) -> (PathBuf, Vec<PathBuf>) {
        let dir = root.join(format!("{}-archive", name));
        let dist_info = format!("{}-1.0.dist-info", name);
        let mut list = Vec::new();
        let mut record = String::new();
        for f in files
            .iter()
            .copied()
            .chain([format!("{}/METADATA", dist_info).as_str()])
        {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, f).unwrap();
            record.push_str(&format!("{},sha256=x,{}\n", f, f.len()));
            list.push(PathBuf::from(f));
        }
        let rec = format!("{}/RECORD", dist_info);
        record.push_str(&format!("{},,\n", rec));
        std::fs::write(dir.join(&rec), record).unwrap();
        list.push(PathBuf::from(rec));
        (dir, list)
    }

    #[test]
    fn installs_whole_trees_and_merges_shared_directories() {
        let root = std::env::temp_dir().join(format!("vyp-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layout = VenvLayout {
            root: root.join("venv"),
            site_packages: root.join("venv/lib/python3.13/site-packages"),
            scripts: root.join("venv/bin"),
            include: root.join("venv/include"),
            python_exe: root.join("venv/bin/python"),
            python_version: "3.13".into(),
            relocatable: false,
        };
        let (a, a_files) = fake_wheel(
            &root,
            "a",
            &["ns/a/__init__.py", "ns/a/deep/mod.py", "a_top.py"],
        );
        let (b, b_files) = fake_wheel(&root, "b", &["ns/b/__init__.py"]);
        install_wheel(&a, &layout, Some(&a_files), true, false).unwrap();
        install_wheel(&b, &layout, Some(&b_files), false, false).unwrap();

        let sp = &layout.site_packages;
        for f in [
            "ns/a/__init__.py",
            "ns/a/deep/mod.py",
            "a_top.py",
            "ns/b/__init__.py",
        ] {
            assert!(sp.join(f).is_file(), "{} missing", f);
        }
        let record = std::fs::read_to_string(sp.join("a-1.0.dist-info/RECORD")).unwrap();
        assert!(record.contains("ns/a/deep/mod.py,sha256=x,"), "{}", record);
        assert!(record.contains("a-1.0.dist-info/INSTALLER"), "{}", record);
        assert!(std::fs::read_dir(sp)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with(".vyp-clone")));
        // Writing into the venv must not touch the cached archive.
        std::fs::write(sp.join("ns/a/__init__.py"), "changed").unwrap();
        assert_eq!(
            std::fs::read_to_string(a.join("ns/a/__init__.py")).unwrap(),
            "ns/a/__init__.py"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
