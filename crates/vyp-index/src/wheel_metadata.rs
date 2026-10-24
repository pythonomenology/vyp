//! Fetch a wheel's `METADATA` without downloading the whole wheel.
//!
//! Many indexes (e.g. `download.pytorch.org`) are PEP 503 HTML repositories
//! that do **not** serve PEP 658 `.metadata` sidecar files. Without this,
//! resolution against such indexes yields empty dependencies. A wheel is just
//! a ZIP archive whose `*.dist-info/METADATA` member holds the `Requires-Dist`
//! lines, so we extract only that member using HTTP range requests:
//!
//! 1. fetch the tail of the file (end-of-central-directory + central directory),
//! 2. locate the `METADATA` entry and its local-header offset,
//! 3. fetch just that member's bytes and inflate them.
//!
//! Typical cost is 2–3 small ranged GETs (a few KiB) instead of a multi-hundred
//! megabyte wheel download. Falls back gracefully: if the server ignores
//! `Range` (returns `200`) we still parse the full body; on any structural
//! surprise (ZIP64, missing member) we return an error and the caller can fall
//! back to a normal download.

use std::io::Read;

use reqwest::header::{CONTENT_RANGE, RANGE};
use tokio::sync::Semaphore;

/// Bytes to request from the tail of the wheel. Comfortably covers the
/// end-of-central-directory record plus the central directory of typical
/// wheels (even large ones list only a few dozen files).
const TAIL_BYTES: u64 = 64 * 1024;
/// Smaller tail for optional members looked up in every selected wheel: it
/// covers most central directories, and a larger one costs one more request.
pub const SMALL_TAIL_BYTES: u64 = 16 * 1024;
/// Slack added when fetching a local member to absorb the local extra field
/// (which may differ in size from the central directory copy).
const LOCAL_SLACK: usize = 4096;

const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;

type DynError = Box<dyn std::error::Error + Send + Sync>;

fn le_u16(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

/// A buffer of bytes from the wheel together with the absolute file offset of
/// its first byte (`base`).
struct Chunk {
    bytes: Vec<u8>,
    base: u64,
}

/// Issue a ranged GET. Returns the body and the absolute offset of its first
/// byte: from `Content-Range` for a `206`, or `0` if the server returned the
/// whole file (`200`).
async fn range_get(client: &reqwest::Client, url: &str, range: &str) -> Result<Chunk, DynError> {
    let mut resp = crate::auth::apply_auth(client.get(url).header(RANGE, range), url)
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        // Some CDNs reject a suffix range longer than the file instead of
        // returning the whole file; such a file is smaller than the range,
        // so just fetch all of it.
        resp = crate::auth::apply_auth(client.get(url), url).send().await?;
    }
    if !resp.status().is_success() {
        return Err(format!("range fetch failed: {}", resp.status()).into());
    }
    let base = if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        resp.headers()
            .get(CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            .and_then(parse_content_range_start)
            .unwrap_or(0)
    } else {
        0 // 200 OK — whole file, base offset 0
    };
    let bytes = resp.bytes().await?.to_vec();
    Ok(Chunk { bytes, base })
}

/// Parse the start offset out of a `Content-Range: bytes START-END/TOTAL` header.
fn parse_content_range_start(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let range = rest.split('/').next()?;
    range.split('-').next()?.trim().parse().ok()
}

/// Last byte offset of `len` bytes starting at `start`, for an inclusive HTTP
/// `Range` end. `None` when the span is empty (`len == 0`) or overflows.
fn inclusive_range_end(start: u64, len: u64) -> Option<u64> {
    if len == 0 {
        return None;
    }
    start.checked_add(len - 1)
}

/// A central-directory entry we care about.
struct CentralEntry {
    method: u16,
    compressed_size: u64,
    local_offset: u64,
}

/// Find the end-of-central-directory record by scanning backwards for its
/// signature, then read the central directory's size and offset.
fn read_eocd(buf: &[u8]) -> Option<(u64, u64)> {
    // EOCD is 22 bytes + comment; scan backwards for the signature.
    if buf.len() < 22 {
        return None;
    }
    for i in (0..=buf.len() - 22).rev() {
        if le_u32(buf, i) == Some(SIG_EOCD) {
            let cd_size = le_u32(buf, i + 12)? as u64;
            let cd_offset = le_u32(buf, i + 16)? as u64;
            return Some((cd_offset, cd_size));
        }
    }
    None
}

/// Scan the central directory for the first entry whose name matches.
fn find_entry(cd: &[u8], matches: impl Fn(&[u8]) -> bool) -> Option<CentralEntry> {
    let mut pos = 0usize;
    while pos + 46 <= cd.len() {
        if le_u32(cd, pos) != Some(SIG_CENTRAL) {
            break;
        }
        let method = le_u16(cd, pos + 10)?;
        let compressed_size = le_u32(cd, pos + 20)? as u64;
        let name_len = le_u16(cd, pos + 28)? as usize;
        let extra_len = le_u16(cd, pos + 30)? as usize;
        let comment_len = le_u16(cd, pos + 32)? as usize;
        let local_offset = le_u32(cd, pos + 42)? as u64;

        let name_start = pos + 46;
        let name = cd.get(name_start..name_start + name_len)?;

        if matches(name) {
            return Some(CentralEntry {
                method,
                compressed_size,
                local_offset,
            });
        }
        pos = name_start + name_len + extra_len + comment_len;
    }
    None
}

/// Inflate a raw DEFLATE member, or return stored bytes verbatim.
fn decompress(method: u16, data: &[u8]) -> Result<String, DynError> {
    match method {
        0 => Ok(String::from_utf8_lossy(data).into_owned()),
        8 => {
            let mut decoder = flate2::read::DeflateDecoder::new(data);
            let mut out = String::new();
            decoder.read_to_string(&mut out)?;
            Ok(out)
        }
        other => Err(format!("unsupported ZIP compression method {}", other).into()),
    }
}

fn is_metadata(name: &[u8]) -> bool {
    name.ends_with(b".dist-info/METADATA")
}

/// True for a `.dist-info/` member named `file_name`, at any depth below the
/// `.dist-info` directory (build backends differ on where extra metadata goes).
pub fn is_dist_info_file(name: &[u8], file_name: &str) -> bool {
    let Ok(name) = std::str::from_utf8(name) else {
        return false;
    };
    let mut parts = name.split('/');
    let in_dist_info = parts.next().is_some_and(|top| top.ends_with(".dist-info"));
    in_dist_info && name.rsplit('/').next() == Some(file_name)
}

/// Fetch the `METADATA` contents of a remote wheel via range requests.
pub async fn fetch_wheel_metadata(
    client: &reqwest::Client,
    wheel_url: &str,
    semaphore: &Semaphore,
) -> Result<String, DynError> {
    fetch_wheel_member(client, wheel_url, semaphore, TAIL_BYTES, is_metadata)
        .await?
        .ok_or_else(|| "METADATA not found in wheel".into())
}

/// Fetch the first member of a remote wheel whose name matches, via range
/// requests. `Ok(None)` means the wheel was read but has no such member.
pub async fn fetch_wheel_member(
    client: &reqwest::Client,
    wheel_url: &str,
    semaphore: &Semaphore,
    tail_bytes: u64,
    matches: impl Fn(&[u8]) -> bool,
) -> Result<Option<String>, DynError> {
    let _permit = semaphore.acquire().await.expect("semaphore closed");

    // 1. Tail: EOCD + (usually) the whole central directory.
    let tail = range_get(client, wheel_url, &format!("bytes=-{}", tail_bytes)).await?;
    let (cd_offset, cd_size) =
        read_eocd(&tail.bytes).ok_or("no end-of-central-directory record")?;
    if cd_offset == 0xFFFF_FFFF || cd_size == 0xFFFF_FFFF {
        return Err("ZIP64 wheel not supported by range fetch".into());
    }

    // 2. Central directory bytes — already in the tail, or a second fetch.
    let cd_bytes: Vec<u8> = if cd_offset >= tail.base {
        let start = (cd_offset - tail.base) as usize;
        tail.bytes
            .get(start..(start + cd_size as usize).min(tail.bytes.len()))
            .ok_or("central directory out of range")?
            .to_vec()
    } else {
        // An empty central directory (e.g. a malformed or hostile EOCD with
        // offset 0 and size 0) has no METADATA; `cd_offset + cd_size - 1`
        // would underflow here.
        let end = inclusive_range_end(cd_offset, cd_size).ok_or("empty central directory")?;
        range_get(client, wheel_url, &format!("bytes={}-{}", cd_offset, end))
            .await?
            .bytes
    };

    let Some(entry) = find_entry(&cd_bytes, matches) else {
        return Ok(None);
    };

    // 3. Fetch the local member (local header + name + extra + compressed data),
    //    with slack for the local extra field, then inflate.
    let want = 30 + entry.compressed_size + LOCAL_SLACK as u64;
    let end = entry.local_offset + want - 1;
    let local = range_get(
        client,
        wheel_url,
        &format!("bytes={}-{}", entry.local_offset, end),
    )
    .await?;

    let lb = &local.bytes;
    let name_len = le_u16(lb, 26).ok_or("short local header")? as usize;
    let extra_len = le_u16(lb, 28).ok_or("short local header")? as usize;
    let data_start = 30 + name_len + extra_len;
    let data_end = data_start + entry.compressed_size as usize;
    let data = lb
        .get(data_start..data_end)
        .ok_or("local member truncated")?;

    decompress(entry.method, data).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_content_range() {
        assert_eq!(parse_content_range_start("bytes 100-199/5000"), Some(100));
        assert_eq!(parse_content_range_start("bytes 0-63/64"), Some(0));
        assert_eq!(parse_content_range_start("garbage"), None);
    }

    #[test]
    fn roundtrip_zip_metadata() {
        // Build a tiny in-memory ZIP with a stored (uncompressed) METADATA
        // member and verify the parser extracts it.
        let metadata = b"Metadata-Version: 2.1\nName: demo\nRequires-Dist: requests\n";
        let zip = build_stored_zip("demo-1.0.dist-info/METADATA", metadata);

        let (cd_offset, cd_size) = read_eocd(&zip).unwrap();
        let cd = &zip[cd_offset as usize..(cd_offset + cd_size) as usize];
        let entry = find_entry(cd, is_metadata).unwrap();
        assert_eq!(entry.method, 0);

        let lb = &zip[entry.local_offset as usize..];
        let name_len = le_u16(lb, 26).unwrap() as usize;
        let extra_len = le_u16(lb, 28).unwrap() as usize;
        let data_start = 30 + name_len + extra_len;
        let data = &lb[data_start..data_start + entry.compressed_size as usize];
        assert_eq!(
            decompress(entry.method, data).unwrap(),
            String::from_utf8_lossy(metadata)
        );
    }

    #[test]
    fn dist_info_file_matching() {
        let hit = |n: &str| is_dist_info_file(n.as_bytes(), "vyp-overrides.toml");
        assert!(hit("demo-1.0.dist-info/vyp-overrides.toml"));
        assert!(hit("demo-1.0.dist-info/extra_metadata/vyp-overrides.toml"));
        assert!(!hit("demo/vyp-overrides.toml"));
        assert!(!hit("demo-1.0.dist-info/not-vyp-overrides.toml"));
    }

    /// Minimal ZIP writer: one stored file. Enough to exercise the parser.
    fn build_stored_zip(name: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let name_b = name.as_bytes();
        let crc = crc32(data);

        // Local file header.
        let local_offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method = stored
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed
        out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed
        out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_b);
        out.extend_from_slice(data);

        // Central directory.
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&SIG_CENTRAL.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&local_offset.to_le_bytes());
        out.extend_from_slice(name_b);
        let cd_size = out.len() as u32 - cd_offset;

        // EOCD.
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    /// Minimal CRC32 (only needed so the test ZIP is well-formed).
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }
}
