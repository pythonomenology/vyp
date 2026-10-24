use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

const FILELIST_MANIFEST: &str = ".vyp_filelist";

/// Default archive cache limit. PyTorch CPU + NumPy + typical deps can exceed 5 GB.
const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 20 * 1024 * 1024 * 1024; // 20 GB

/// Two-tier wheel cache:
///   wheels/   — raw .whl files keyed by cache key
///   archive/  — pre-extracted wheel contents keyed by cache key
///
/// Cache key is the wheel filename (e.g. "requests-2.32.5-py3-none-any.whl")
/// which is unique per distribution and always available from the resolver.
pub struct WheelCache {
    base_dir: PathBuf,
    archive_dir: PathBuf,
    max_archive_bytes: u64,
}

impl WheelCache {
    pub fn new() -> Self {
        let base = cache_base_dir();
        let archive_dir = base.join("archive");
        let max_archive_bytes = std::env::var("VYP_CACHE_MAX_ARCHIVE_GB")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|gb| gb * 1024 * 1024 * 1024)
            .unwrap_or(DEFAULT_MAX_ARCHIVE_BYTES);
        Self { base_dir: base, archive_dir, max_archive_bytes }
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.base_dir.join("tmp")
    }

    #[allow(dead_code)]
    pub fn get_archive(&self, key: &str) -> Option<PathBuf> {
        self.get_archive_with_file_list(key).map(|(path, _)| path)
    }

    /// Like get_archive but also loads the persisted file list if present, so the
    /// linker can skip WalkDir on cache hits.
    pub fn get_archive_with_file_list(&self, key: &str) -> Option<(PathBuf, Option<Vec<PathBuf>>)> {
        let dir = self.archive_dir.join(key);
        if !dir.is_dir() {
            return None;
        }
        touch_dir_mtime(&dir);
        let list = read_filelist_manifest(&dir);
        Some((dir, list))
    }

    /// Extract a wheel file from disk into archive cache.
    #[allow(dead_code)]
    pub fn store_from_file(&self, key: &str, wheel_path: &Path) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        let (path, _) = self.store_from_file_with_list(key, wheel_path, true)?;
        Ok(path)
    }

    /// Extract a wheel and return both the archive path and the list of
    /// relative file paths. The file list can be passed directly to the
    /// linker, skipping the walkdir traversal.
    /// When `evict_after` is false, eviction is skipped (caller should run
    /// `evict_archives_if_needed()` once after a batch of installs).
    pub fn store_from_file_with_list(
        &self,
        key: &str,
        wheel_path: &Path,
        evict_after: bool,
    ) -> Result<(PathBuf, Vec<PathBuf>), Box<dyn std::error::Error + Send + Sync>> {
        let archive_path = self.archive_dir.join(key);
        if archive_path.is_dir() {
            return Ok((archive_path, Vec::new()));
        }

        let tmp_path = self.archive_dir.join(format!(".{}.tmp", key));
        let _ = std::fs::remove_dir_all(&tmp_path);
        std::fs::create_dir_all(&tmp_path)?;

        let file = std::fs::File::open(wheel_path)?;
        let file_list = extract_zip(file, &tmp_path)?;

        std::fs::rename(&tmp_path, &archive_path)?;

        write_filelist_manifest(&archive_path, &file_list)?;

        if evict_after {
            self.evict_archives_if_needed();
        }

        Ok((archive_path, file_list))
    }

    /// Run LRU eviction if cache size exceeds the limit. Call once after a
    /// batch of installs instead of after each package to avoid O(n²) work.
    pub fn evict_archives_if_needed(&self) {
        let Ok(entries) = std::fs::read_dir(&self.archive_dir) else { return };

        let mut dirs: Vec<(PathBuf, u64, u64)> = Vec::new();
        let mut total_size: u64 = 0;

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let size = dir_size_fast(&entry.path());
            let mtime = entry.metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            total_size += size;
            dirs.push((entry.path(), mtime, size));
        }

        if total_size <= self.max_archive_bytes {
            return;
        }

        dirs.sort_by_key(|(_, mtime, _)| *mtime);

        for (path, _, size) in &dirs {
            if total_size <= self.max_archive_bytes {
                break;
            }
            let _ = std::fs::remove_dir_all(path);
            total_size = total_size.saturating_sub(*size);
        }
    }
}

fn extract_zip<R: std::io::Read + std::io::Seek>(
    reader: R,
    dest: &Path,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error + Send + Sync>> {
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut file_list = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let rel = file.mangled_name();
        let outpath = dest.join(&rel);

        if file.name().ends_with('/') {
            std::fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut outfile = std::fs::File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }

        file_list.push(rel);
    }

    Ok(file_list)
}

fn touch_dir_mtime(path: &Path) {
    let now = filetime::FileTime::now();
    let _ = filetime::set_file_mtime(path, now);
}

fn dir_size_fast(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn write_filelist_manifest(archive_path: &Path, file_list: &[PathBuf]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let manifest = archive_path.join(FILELIST_MANIFEST);
    let content: String = file_list
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(manifest, content)?;
    Ok(())
}

fn read_filelist_manifest(archive_path: &Path) -> Option<Vec<PathBuf>> {
    let manifest = archive_path.join(FILELIST_MANIFEST);
    let f = std::fs::File::open(manifest).ok()?;
    let paths: Vec<PathBuf> = BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    if paths.is_empty() {
        return None;
    }
    Some(paths)
}

pub fn cache_base_dir() -> PathBuf {
    std::env::var("VYP_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                    PathBuf::from(home).join(".cache")
                })
                .join("vyp")
        })
}
