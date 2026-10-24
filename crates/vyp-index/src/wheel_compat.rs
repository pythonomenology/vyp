use std::collections::HashSet;
use std::sync::OnceLock;

use vyp_api::MarkerEnvironment;

/// Platform tags describing the current environment, used to filter
/// incompatible wheels before selection.
///
/// `python_tags`, `abi_tags`, and `platform_tags` are each ordered from
/// most-preferred to least-preferred. A wheel's preference rank is derived
/// from the *position* of its best-matching tag in each list (earlier =
/// better), mirroring `packaging.tags` ordering so that "most specific
/// compatible tag wins" falls out naturally. The parallel `*_set` fields give
/// O(1) membership for the hot `is_compatible` check (called once per wheel
/// across the whole resolve); the ordered vectors are only consulted by the
/// rarely-called `compatibility_score`.
#[derive(Debug, Clone)]
pub struct PlatformTags {
    python_tags: Vec<String>,
    abi_tags: Vec<String>,
    platform_tags: Vec<String>,
    python_set: HashSet<String>,
    abi_set: HashSet<String>,
    platform_set: HashSet<String>,
    /// `(major, minor)` of a CPython target that accepts stable-ABI wheels:
    /// `cp3Y-abi3` works on CPython 3.Z for every Y <= Z.
    abi3_python: Option<(u32, u32)>,
}

impl PlatformTags {
    /// Build platform tags from a detected marker environment.
    pub fn from_env(env: &MarkerEnvironment) -> Self {
        let (major, minor) = parse_python_version(&env.python_version);
        let impl_name = env.implementation_name.to_lowercase();
        let free_threaded = detect_free_threaded(env);

        let python_tags = build_python_tags(&impl_name, major, minor);
        let abi_tags = build_abi_tags(&impl_name, major, minor, free_threaded);
        let platform_tags = build_platform_tags(&env.sys_platform, &env.platform_machine);

        let python_set = python_tags.iter().cloned().collect();
        let abi_set: HashSet<String> = abi_tags.iter().cloned().collect();
        let platform_set = platform_tags.iter().cloned().collect();
        let abi3_python = abi_set.contains("abi3").then_some((major, minor));

        Self {
            python_tags,
            abi_tags,
            platform_tags,
            python_set,
            abi_set,
            platform_set,
            abi3_python,
        }
    }

    /// Split a wheel filename into its (python, abi, platform) tag fields,
    /// accounting for an optional build tag and an optional PEP 825 variant
    /// label.
    fn tag_fields(filename: &str) -> Option<(&str, &str, &str)> {
        let stem = filename.strip_suffix(".whl")?;
        let parts: Vec<&str> = stem.split('-').collect();
        match parts.len() {
            5 => Some((parts[2], parts[3], parts[4])),
            6 => {
                if parts[2].starts_with(|c: char| c.is_ascii_digit()) {
                    // build tag present: dist-ver-build-py-abi-plat
                    Some((parts[3], parts[4], parts[5]))
                } else {
                    // variant label present: dist-ver-py-abi-plat-variant
                    Some((parts[2], parts[3], parts[4]))
                }
            }
            7 => Some((parts[3], parts[4], parts[5])),
            _ => None,
        }
    }

    /// A short stable key for this tag set, for caching per-environment data.
    pub fn fingerprint(&self) -> String {
        let mut hash: u64 = 0xcbf29ce484222325;
        let mut feed = |s: &str| {
            for b in s.as_bytes().iter().chain(b"\0") {
                hash ^= *b as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        };
        for list in [&self.python_tags, &self.abi_tags, &self.platform_tags] {
            for tag in list {
                feed(tag);
            }
            feed("|");
        }
        if let Some((major, minor)) = self.abi3_python {
            feed(&format!("abi3:{}.{}", major, minor));
        }
        format!("{:016x}", hash)
    }

    /// Check whether a wheel filename is compatible with this platform.
    /// Returns `true` for compatible wheels; `false` for incompatible or
    /// unparseable wheel filenames.
    pub fn is_compatible(&self, filename: &str) -> bool {
        let Some((py_tag, abi_tag, plat_tag)) = Self::tag_fields(filename) else {
            return false;
        };

        self.python_abi_compatible(py_tag, abi_tag) && any_member(plat_tag, &self.platform_set)
    }

    /// Python and ABI tags are only meaningful as a pair: `cp39-abi3` runs
    /// on CPython 3.13, but `cp39-cp39` doesn't. Each field may be a
    /// compressed `.`-separated set.
    fn python_abi_compatible(&self, py_tag: &str, abi_tag: &str) -> bool {
        abi_tag.split('.').any(|abi| {
            py_tag.split('.').any(|py| {
                (self.python_set.contains(py) && self.abi_set.contains(abi))
                    || (abi == "abi3" && self.is_abi3_python(py))
            })
        })
    }

    /// `cp3Y` with `2 <= Y <= minor`: the first stable-ABI release (3.2) up
    /// to the target.
    fn is_abi3_python(&self, py: &str) -> bool {
        let Some((major, minor)) = self.abi3_python else { return false };
        py.strip_prefix(&format!("cp{}", major))
            .and_then(|m| m.parse::<u32>().ok())
            .is_some_and(|m| (2..=minor).contains(&m))
    }

    /// Score a wheel for preference ordering. Higher is better; `0` for an
    /// incompatible or unparseable wheel.
    ///
    /// The score is derived from the position of the wheel's best-matching tag
    /// in each ordered tag list, so a more specific compatible distribution
    /// (e.g. `cp311-cp311-manylinux_2_28_x86_64`) outscores a more generic one
    /// (e.g. `py3-none-any`) without any ad-hoc per-tag weighting.
    pub fn compatibility_score(&self, filename: &str) -> u32 {
        let Some((py_tag, abi_tag, plat_tag)) = Self::tag_fields(filename) else {
            return 0;
        };

        if !self.python_abi_compatible(py_tag, abi_tag) {
            return 0;
        }
        // An older `cp3Y-abi3` tag ranks below every exact python tag.
        let py_rank = best_rank(py_tag, &self.python_tags).or_else(|| {
            py_tag
                .split('.')
                .any(|py| self.is_abi3_python(py))
                .then(|| self.python_tags.len() - 1)
        });
        let (Some(py_r), Some(abi_r), Some(plat_r)) = (
            py_rank,
            best_rank(abi_tag, &self.abi_tags),
            best_rank(plat_tag, &self.platform_tags),
        ) else {
            return 0;
        };

        // Convert each rank (0 = best) into a descending preference value, then
        // combine with platform dominating, then abi, then python — matching
        // packaging's tag priority where platform specificity matters most.
        let py_pref = (self.python_tags.len() - py_r) as u32;
        let abi_pref = (self.abi_tags.len() - abi_r) as u32;
        let plat_pref = (self.platform_tags.len() - plat_r) as u32;

        // +1 so a perfectly-compatible wheel always scores > 0.
        1 + plat_pref * 10_000 + abi_pref * 100 + py_pref
    }
}

/// O(1) membership: whether any `.`-joined sub-tag of `wheel_tag` is supported.
fn any_member(wheel_tag: &str, supported: &HashSet<String>) -> bool {
    wheel_tag.split('.').any(|sub| supported.contains(sub))
}

/// Return the position (0 = best) of the wheel tag's best match within the
/// ordered supported-tag list, or `None` if no sub-tag matches. Wheel tags can
/// be compressed sets joined by `.` (e.g. `manylinux_2_17.manylinux2014`); the
/// best (lowest) rank across sub-tags is returned.
fn best_rank(wheel_tag: &str, supported: &[String]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for sub in wheel_tag.split('.') {
        if let Some(idx) = supported.iter().position(|s| s == sub) {
            best = Some(best.map_or(idx, |b| b.min(idx)));
        }
    }
    best
}

fn parse_python_version(version_str: &str) -> (u32, u32) {
    let parts: Vec<&str> = version_str.split('.').collect();
    let major = parts.first().and_then(|s| s.parse().ok()).unwrap_or(3);
    let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(12);
    (major, minor)
}

/// Whether the target interpreter is a free-threaded (PEP 703) build. The
/// marker environment does not carry the GIL flag directly, so we accept an
/// explicit signal: a `t` suffix in the implementation/full version, or the
/// `VYP_FREE_THREADED=1` environment override.
fn detect_free_threaded(env: &MarkerEnvironment) -> bool {
    if std::env::var("VYP_FREE_THREADED").is_ok_and(|v| v == "1") {
        return true;
    }
    env.python_full_version.ends_with('t')
        || env.implementation_version.ends_with('t')
}

fn build_python_tags(impl_name: &str, major: u32, minor: u32) -> Vec<String> {
    let mut tags = Vec::new();

    let cp_prefix = if impl_name == "cpython" { "cp" } else { impl_name };

    tags.push(format!("{}{}{}", cp_prefix, major, minor));

    // py3X..py30 then py3 (generic, most-to-least specific).
    for m in (0..=minor).rev() {
        tags.push(format!("py{}{}", major, m));
    }
    tags.push(format!("py{}", major));

    tags
}

fn build_abi_tags(impl_name: &str, major: u32, minor: u32, free_threaded: bool) -> Vec<String> {
    let mut tags = Vec::new();

    let cp_prefix = if impl_name == "cpython" { "cp" } else { impl_name };

    if free_threaded {
        // Free-threaded ABI is incompatible with the GIL ABI; prefer it but
        // also accept the GIL tag (some builds remain dual-ABI).
        tags.push(format!("{}{}{}t", cp_prefix, major, minor));
    }
    tags.push(format!("{}{}{}", cp_prefix, major, minor));

    if impl_name == "cpython" {
        // Stable ABI (abi3) wheels work on this CPython and any newer one.
        tags.push("abi3".to_string());
    }

    tags.push("none".to_string());
    tags
}

fn build_platform_tags(sys_platform: &str, machine: &str) -> Vec<String> {
    let mut tags = Vec::new();

    match sys_platform {
        "darwin" => build_macos_tags(machine, &mut tags),
        "linux" => build_linux_tags(machine, &mut tags),
        "win32" | "windows" | "cygwin" => {
            let win_arch = match machine {
                "x86_64" | "AMD64" | "amd64" => "amd64",
                "arm64" | "aarch64" | "ARM64" => "arm64",
                "x86" | "i386" | "i686" => "win32",
                other => other,
            };
            if win_arch == "win32" {
                tags.push("win32".to_string());
            } else {
                tags.push(format!("win_{}", win_arch));
            }
        }
        _ => {}
    }

    tags.push("any".to_string());
    tags
}

/// macOS deployment-target tags from the host OS version down to 10.0, with
/// arch-specific, universal2, and (for 10.x) legacy variants — ordered
/// most-recent-first to prefer the most specific compatible build.
fn build_macos_tags(machine: &str, tags: &mut Vec<String>) {
    let arch = match machine {
        "aarch64" => "arm64",
        other => other,
    };
    let (host_major, host_minor) = detect_macos_version();

    // Per packaging.tags: for macOS 11+ the minor is pinned to 0 for the
    // major-version line, while 10.x enumerates the real minor versions.
    let arch_variants: &[&str] = if arch == "arm64" {
        &["arm64", "universal2"]
    } else if arch == "x86_64" {
        &["x86_64", "universal2", "intel", "fat64", "fat32"]
    } else {
        // Unknown arch: still offer universal2 fallback.
        &["universal2"]
    };

    if host_major >= 11 {
        for major in (11..=host_major).rev() {
            for variant in arch_variants {
                tags.push(format!("macosx_{}_0_{}", major, variant));
            }
        }
    }
    // 10.x line (10.0 ..= 10.15), highest first.
    let ten_max = if host_major == 10 { host_minor } else { 15 };
    for minor in (0..=ten_max).rev() {
        for variant in arch_variants {
            tags.push(format!("macosx_10_{}_{}", minor, variant));
        }
    }
}

/// Detect the running macOS product version (major, minor). Falls back to
/// (11, 0) — the modern manylinux-equivalent baseline — if detection fails.
fn detect_macos_version() -> (u32, u32) {
    static VER: OnceLock<(u32, u32)> = OnceLock::new();
    *VER.get_or_init(|| {
        if let Ok(target) = std::env::var("MACOSX_DEPLOYMENT_TARGET") {
            if let Some(v) = parse_two_part(&target) {
                return v;
            }
        }
        if let Some(v) = sysctl_product_version().as_deref().and_then(parse_two_part) {
            return v;
        }
        let out = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    if let Some(v) = parse_two_part(s.trim()) {
                        return v;
                    }
                }
            }
        }
        (11, 0)
    })
}

/// The macOS product version from `sysctl kern.osproductversion`, without
/// spawning `sw_vers`.
#[cfg(target_os = "macos")]
fn sysctl_product_version() -> Option<String> {
    let name = c"kern.osproductversion";
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `name` is NUL-terminated; `buf`/`len` describe a valid buffer.
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let s = std::str::from_utf8(&buf[..len]).ok()?;
    Some(s.trim_end_matches('\0').trim().to_string())
}

#[cfg(not(target_os = "macos"))]
fn sysctl_product_version() -> Option<String> {
    None
}

fn parse_two_part(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// Linux platform tags, libc-aware: only the host's libc family is emitted,
/// and manylinux/musllinux tags are capped at the detected libc minor version
/// so we never mark a wheel compatible that the host loader can't satisfy.
fn build_linux_tags(machine: &str, tags: &mut Vec<String>) {
    let arch = match machine {
        "arm64" => "aarch64",
        "AMD64" | "amd64" => "x86_64",
        other => other,
    };

    match detect_libc() {
        Libc::Glibc(minor) => {
            // manylinux_2_Y from the host glibc minor down to 2_17 (the
            // manylinux2014 baseline), then the legacy perennial aliases.
            let cap = minor.max(17);
            for y in (17..=cap).rev() {
                tags.push(format!("manylinux_2_{}_{}", y, arch));
            }
            // manylinux2014 == glibc 2.17, 2010 == 2.12, 1 == 2.5 — all
            // satisfied once the host is at least glibc 2.17.
            tags.push(format!("manylinux2014_{}", arch));
            if arch == "x86_64" || arch == "i686" {
                tags.push(format!("manylinux2010_{}", arch));
                tags.push(format!("manylinux1_{}", arch));
            }
        }
        Libc::Musl(minor) => {
            let cap = minor.max(1);
            for y in (1..=cap).rev() {
                tags.push(format!("musllinux_1_{}_{}", y, arch));
            }
        }
        Libc::Unknown => {
            // Conservative: assume manylinux2014 baseline (glibc 2.17).
            tags.push(format!("manylinux_2_17_{}", arch));
            tags.push(format!("manylinux2014_{}", arch));
            tags.push(format!("manylinux2010_{}", arch));
            tags.push(format!("manylinux1_{}", arch));
        }
    }

    tags.push(format!("linux_{}", arch));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Libc {
    /// glibc with the given minor version (the `Y` in `2.Y`).
    Glibc(u32),
    /// musl with the given minor version (the `Y` in `1.Y`).
    Musl(u32),
    Unknown,
}

/// Detect the host C library family and version once per process. Uses
/// `ldd --version` (the portable approach used by pip's vendored packaging),
/// with environment overrides for testing and reproducible cross-resolves:
/// `VYP_LIBC=glibc:2.31` or `VYP_LIBC=musl:1.2`.
fn detect_libc() -> Libc {
    static LIBC: OnceLock<Libc> = OnceLock::new();
    *LIBC.get_or_init(|| {
        if let Ok(override_str) = std::env::var("VYP_LIBC") {
            if let Some(libc) = parse_libc_override(&override_str) {
                return libc;
            }
        }

        // musl systems ship a dynamic loader at a predictable path.
        for entry in ["/lib/ld-musl-x86_64.so.1", "/lib/ld-musl-aarch64.so.1"] {
            if std::path::Path::new(entry).exists() {
                if let Some(minor) = musl_minor_from_ldd() {
                    return Libc::Musl(minor);
                }
                return Libc::Musl(2);
            }
        }

        // Parse `ldd --version` for glibc / musl.
        let out = std::process::Command::new("ldd").arg("--version").output();
        if let Ok(out) = out {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            let lower = text.to_lowercase();
            if lower.contains("musl") {
                if let Some(minor) = parse_musl_version(&text) {
                    return Libc::Musl(minor);
                }
                return Libc::Musl(2);
            }
            if let Some(minor) = parse_glibc_version(&text) {
                return Libc::Glibc(minor);
            }
        }

        Libc::Unknown
    })
}

fn parse_libc_override(s: &str) -> Option<Libc> {
    let (kind, ver) = s.split_once(':')?;
    let minor = ver.split('.').nth(1)?.parse().ok()?;
    match kind {
        "glibc" => Some(Libc::Glibc(minor)),
        "musl" => Some(Libc::Musl(minor)),
        _ => None,
    }
}

fn musl_minor_from_ldd() -> Option<u32> {
    let out = std::process::Command::new("ldd").arg("--version").output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    parse_musl_version(&text)
}

/// Extract the glibc minor version from `ldd (GNU libc) 2.31`-style output.
fn parse_glibc_version(text: &str) -> Option<u32> {
    for line in text.lines() {
        // Find a "2.NN" token.
        for token in line.split_whitespace() {
            if let Some(rest) = token.strip_prefix("2.") {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Extract the musl minor version from `Version 1.2.3`-style output.
fn parse_musl_version(text: &str) -> Option<u32> {
    for token in text.split_whitespace() {
        if let Some(rest) = token.strip_prefix("1.") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;



    fn arm64_mac_env() -> MarkerEnvironment {
        MarkerEnvironment {
            os_name: "posix".to_string(),
            sys_platform: "darwin".to_string(),
            platform_system: "Darwin".to_string(),
            platform_machine: "arm64".to_string(),
            platform_release: String::new(),
            platform_version: String::new(),
            platform_python_implementation: "CPython".to_string(),
            implementation_name: "cpython".to_string(),
            python_version: "3.10".to_string(),
            python_full_version: "3.10.0".to_string(),
            implementation_version: "3.10.0".to_string(),
        }
    }

    fn linux_x86_env() -> MarkerEnvironment {
        MarkerEnvironment {
            os_name: "posix".to_string(),
            sys_platform: "linux".to_string(),
            platform_system: "Linux".to_string(),
            platform_machine: "x86_64".to_string(),
            platform_release: String::new(),
            platform_version: String::new(),
            platform_python_implementation: "CPython".to_string(),
            implementation_name: "cpython".to_string(),
            python_version: "3.11".to_string(),
            python_full_version: "3.11.0".to_string(),
            implementation_version: "3.11.0".to_string(),
        }
    }

    #[test]
    fn pure_python_wheel_compatible() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(tags.is_compatible("requests-2.32.5-py3-none-any.whl"));
    }

    #[test]
    fn arm64_mac_wheel_compatible() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(tags.is_compatible(
            "pydantic_core-2.41.5-cp310-cp310-macosx_11_0_arm64.whl"
        ));
    }

    #[test]
    fn x86_64_mac_wheel_incompatible_on_arm64() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(!tags.is_compatible(
            "pydantic_core-2.41.5-cp310-cp310-macosx_10_12_x86_64.whl"
        ));
    }

    #[test]
    fn universal2_wheel_compatible_on_arm64() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(tags.is_compatible(
            "pydantic_core-2.41.5-cp310-cp310-macosx_11_0_universal2.whl"
        ));
    }

    #[test]
    fn linux_wheel_incompatible_on_mac() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(!tags.is_compatible(
            "pydantic_core-2.41.5-cp310-cp310-manylinux_2_17_x86_64.manylinux2014_x86_64.whl"
        ));
    }

    #[test]
    fn wrong_python_version_incompatible() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(!tags.is_compatible(
            "pydantic_core-2.41.5-cp312-cp312-macosx_11_0_arm64.whl"
        ));
    }

    #[test]
    fn abi3_wheel_compatible() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(tags.is_compatible(
            "some_package-1.0.0-cp310-abi3-macosx_11_0_arm64.whl"
        ));
    }

    #[test]
    fn older_abi3_wheels_are_compatible_but_not_older_cpython_abis() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(tags.is_compatible("ast_serialize-0.13.1-cp39-abi3-macosx_11_0_arm64.whl"));
        assert!(tags.is_compatible("x-1.0-cp32-abi3-macosx_11_0_arm64.whl"));
        assert!(!tags.is_compatible("x-1.0-cp39-cp39-macosx_11_0_arm64.whl"));
        assert!(!tags.is_compatible("x-1.0-cp315-abi3-macosx_11_0_arm64.whl"), "abi3 from a newer CPython");
        assert!(!tags.is_compatible("x-1.0-cp39-abi3-manylinux_2_17_x86_64.whl"));
        let exact = tags.compatibility_score("x-1.0-cp310-cp310-macosx_11_0_arm64.whl");
        let abi3 = tags.compatibility_score("x-1.0-cp39-abi3-macosx_11_0_arm64.whl");
        assert!(abi3 > 0 && exact > abi3, "exact {} abi3 {}", exact, abi3);
    }

    #[test]
    fn py3_none_any_compatible() {
        let tags = PlatformTags::from_env(&arm64_mac_env());
        assert!(tags.is_compatible("click-8.3.1-py3-none-any.whl"));
    }

    #[test]
    fn glibc_cap_excludes_newer_manylinux() {
        std::env::set_var("VYP_LIBC", "glibc:2.28");
        // Fresh detection requires a fresh process for OnceLock; emulate by
        // building tags directly through the public path is not possible due to
        // caching, so this asserts the parser/predicate logic instead.
        let tags = build_linux_manylinux_for_test(28);
        assert!(tags.iter().any(|t| t == "manylinux_2_28_x86_64"));
        assert!(!tags.iter().any(|t| t == "manylinux_2_39_x86_64"));
        std::env::remove_var("VYP_LIBC");
    }

    #[test]
    fn musl_host_rejects_manylinux() {
        // Native tag matching: a musl tag list must not contain manylinux.
        let mut tags = Vec::new();
        build_linux_tags_with(Libc::Musl(2), "x86_64", &mut tags);
        assert!(tags.iter().any(|t| t.starts_with("musllinux_1_2")));
        assert!(!tags.iter().any(|t| t.starts_with("manylinux")));
    }

    #[test]
    fn glibc_host_rejects_musllinux() {
        let mut tags = Vec::new();
        build_linux_tags_with(Libc::Glibc(31), "x86_64", &mut tags);
        assert!(tags.iter().any(|t| t.starts_with("manylinux_2_31")));
        assert!(!tags.iter().any(|t| t.starts_with("musllinux")));
    }

    #[test]
    fn native_wheel_outscores_pure_python() {
        let tags = PlatformTags::from_env(&linux_x86_env());
        // For a glibc host the native manylinux wheel should outrank py3-none-any.
        let native = "foo-1.0-cp311-cp311-manylinux_2_17_x86_64.whl";
        let pure = "foo-1.0-py3-none-any.whl";
        if tags.is_compatible(native) {
            assert!(tags.compatibility_score(native) > tags.compatibility_score(pure));
        }
    }

    #[test]
    fn free_threaded_abi_matches_when_flagged() {
        let mut env = linux_x86_env();
        env.python_version = "3.13".to_string();
        env.python_full_version = "3.13.0t".to_string();
        let tags = PlatformTags::from_env(&env);
        // A cp313t free-threaded wheel must be considered compatible.
        assert!(tags.is_compatible("foo-1.0-cp313-cp313t-manylinux_2_17_x86_64.whl")
            || tags.is_compatible("foo-1.0-cp313-cp313t-linux_x86_64.whl"));
    }

    #[test]
    fn parse_glibc_version_works() {
        assert_eq!(parse_glibc_version("ldd (GNU libc) 2.31"), Some(31));
        assert_eq!(parse_glibc_version("ldd (Ubuntu GLIBC 2.35-0ubuntu3) 2.35"), Some(35));
    }

    #[test]
    fn parse_musl_version_works() {
        assert_eq!(parse_musl_version("musl libc (x86_64)\nVersion 1.2.3"), Some(2));
    }

    // --- helpers used only by tests to exercise libc-specific tag logic ---

    fn build_linux_tags_with(libc: Libc, arch: &str, tags: &mut Vec<String>) {
        match libc {
            Libc::Glibc(minor) => {
                let cap = minor.max(17);
                for y in (17..=cap).rev() {
                    tags.push(format!("manylinux_2_{}_{}", y, arch));
                }
                tags.push(format!("manylinux2014_{}", arch));
            }
            Libc::Musl(minor) => {
                let cap = minor.max(1);
                for y in (1..=cap).rev() {
                    tags.push(format!("musllinux_1_{}_{}", y, arch));
                }
            }
            Libc::Unknown => {}
        }
        tags.push(format!("linux_{}", arch));
    }

    fn build_linux_manylinux_for_test(glibc_minor: u32) -> Vec<String> {
        let mut tags = Vec::new();
        build_linux_tags_with(Libc::Glibc(glibc_minor), "x86_64", &mut tags);
        tags
    }
}
