//! Regression tests for the fetch-slot protocol between the solver thread and
//! the async fetch tasks (see tla/FetchSlot.tla).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vyp_api::traits::metadata_provider::MetadataProvider;
use vyp_api::{VypPackage, VypVersion};
use vyp_index::in_memory_index::{InMemoryIndex, MetadataClaim, MetadataResult};
use vyp_index::pypi::PyPIMetadataProvider;

/// Point the metadata disk cache at a fresh directory so every run is cold.
fn cold_cache() {
    static DIR: OnceLock<()> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("vyp-fetch-slots-{}", std::process::id()));
        std::env::set_var("VYP_CACHE_DIR", dir);
    });
}

/// End-of-central-directory record claiming an empty central directory at
/// `cd_offset`.
fn empty_eocd(cd_offset: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    b.extend_from_slice(&[0u8; 8]);
    b.extend_from_slice(&0u32.to_le_bytes()); // cd_size
    b.extend_from_slice(&cd_offset.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes()); // comment length
    b
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    extra_headers: String,
    body: Vec<u8>,
}

/// Serve fixed responses by path on a background runtime; returns the port.
fn serve(
    rt: &tokio::runtime::Runtime,
    routes: impl Fn(u16) -> HashMap<String, Response> + Send + 'static,
) -> u16 {
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let routes = Arc::new(routes(port));
    rt.spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let routes = Arc::clone(&routes);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let not_found = Response {
                    status: "404 Not Found",
                    content_type: "text/plain",
                    extra_headers: String::new(),
                    body: b"not found".to_vec(),
                };
                let r = routes.get(&path).unwrap_or(&not_found);
                let head = format!(
                    "HTTP/1.1 {}\r\nContent-Type: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    r.status, r.content_type, r.extra_headers, r.body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&r.body).await;
            });
        }
    });
    port
}

fn ok(content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
    Response {
        status: "200 OK",
        content_type,
        extra_headers: String::new(),
        body: body.into(),
    }
}

fn simple_json(port: u16, name: &str, sidecar: bool) -> Response {
    ok(
        "application/vnd.pypi.simple.v1+json",
        format!(
            r#"{{"versions":["1.0"],"files":[{{"filename":"{n}-1.0-py3-none-any.whl","url":"http://127.0.0.1:{p}/files/{n}-1.0-py3-none-any.whl","core-metadata":{s}}}]}}"#,
            n = name,
            p = port,
            s = sidecar
        ),
    )
}

/// Run `f` on another thread and fail instead of hanging if it blocks.
fn within_10s<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("call blocked for 10s: a fetch slot was never filled")
}

/// An exact-pin override resolves a package without calling
/// `available_versions`, so `get_metadata` is the first call for it. It must
/// still find the PEP 658 sidecar instead of returning no dependencies.
#[test]
fn metadata_before_versions_uses_sidecar() {
    cold_cache();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let port = serve(&rt, |port| {
        HashMap::from([
            (
                "/simple/pinsidecar/".to_string(),
                simple_json(port, "pinsidecar", true),
            ),
            (
                "/files/pinsidecar-1.0-py3-none-any.whl.metadata".to_string(),
                ok(
                    "text/plain",
                    "Metadata-Version: 2.1\nName: pinsidecar\nVersion: 1.0\nRequires-Dist: leaf\n",
                ),
            ),
        ])
    });
    let provider = PyPIMetadataProvider::new(&format!("http://127.0.0.1:{}/simple", port));
    let meta = within_10s(move || {
        let v: VypVersion = "1.0".parse().unwrap();
        provider
            .get_metadata(&VypPackage::named("pinsidecar"), &v)
            .map_err(|e| e.to_string())
    })
    .expect("get_metadata")
    .expect("metadata present");
    let deps: Vec<_> = meta
        .dependencies
        .iter()
        .map(|r| r.package.name().to_string())
        .collect();
    assert_eq!(deps, ["leaf"]);
}

/// A wheel whose EOCD claims an empty central directory at offset 0, served
/// as the 22-byte tail of a larger file, used to underflow
/// `cd_offset + cd_size - 1` in `fetch_wheel_metadata`. The panic killed the
/// fetch task and left `get_metadata` blocked forever.
#[test]
fn empty_central_directory_is_an_error_not_a_hang() {
    cold_cache();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let port = serve(&rt, |port| {
        HashMap::from([
            (
                "/simple/emptycd/".to_string(),
                simple_json(port, "emptycd", false),
            ),
            (
                "/files/emptycd-1.0-py3-none-any.whl".to_string(),
                Response {
                    status: "206 Partial Content",
                    content_type: "application/octet-stream",
                    extra_headers: "Content-Range: bytes 100-121/122\r\n".to_string(),
                    body: empty_eocd(0),
                },
            ),
            (
                "/files/emptycd50-1.0-py3-none-any.whl".to_string(),
                Response {
                    status: "206 Partial Content",
                    content_type: "application/octet-stream",
                    extra_headers: "Content-Range: bytes 100-121/122\r\n".to_string(),
                    body: empty_eocd(50),
                },
            ),
        ])
    });

    // Offset 0 used to underflow; a non-zero offset used to request the
    // inverted range `bytes=50-49`.
    for file in ["emptycd", "emptycd50"] {
        let client = reqwest::Client::new();
        let url = format!(
            "http://127.0.0.1:{}/files/{}-1.0-py3-none-any.whl",
            port, file
        );
        let direct = rt.block_on(async {
            let sem = tokio::sync::Semaphore::new(1);
            vyp_index::wheel_metadata::fetch_wheel_metadata(&client, &url, &sem)
                .await
                .map_err(|e| e.to_string())
        });
        assert_eq!(direct.unwrap_err(), "empty central directory", "{}", file);
    }

    let provider = PyPIMetadataProvider::new(&format!("http://127.0.0.1:{}/simple", port));
    let meta = within_10s(move || {
        let v: VypVersion = "1.0".parse().unwrap();
        provider
            .get_metadata(&VypPackage::named("emptycd"), &v)
            .map_err(|e| e.to_string())
    });
    assert!(
        matches!(meta, Ok(None)),
        "unexpected result: {:?}",
        meta.map(|m| m.is_some())
    );
}

/// When the version list itself fails, `get_metadata` reports that failure
/// instead of an empty dependency list.
#[test]
fn metadata_reports_failed_version_list() {
    cold_cache();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let port = serve(&rt, |_| {
        HashMap::from([(
            "/simple/vfail/".to_string(),
            Response {
                status: "500 Internal Server Error",
                content_type: "text/plain",
                extra_headers: String::new(),
                body: b"boom".to_vec(),
            },
        )])
    });
    let provider = PyPIMetadataProvider::new(&format!("http://127.0.0.1:{}/simple", port));
    let meta = within_10s(move || {
        let v: VypVersion = "1.0".parse().unwrap();
        provider
            .get_metadata(&VypPackage::named("vfail"), &v)
            .map(|m| m.is_some())
            .map_err(|e| e.to_string())
    });
    let err = meta.unwrap_err();
    assert!(
        err.contains("failed to fetch versions for vfail"),
        "{}",
        err
    );
}

/// A task that panics after claiming a slot must still wake its waiters.
#[test]
fn panicking_claimant_fills_slot_with_error() {
    let index = Arc::new(InMemoryIndex::new());
    let v: VypVersion = "1.0".parse().unwrap();
    assert!(index.register_metadata("boom", &v));
    let claim = MetadataClaim::new(Arc::clone(&index), "boom", &v);
    let worker = std::thread::spawn(move || {
        let _claim = claim;
        panic!("fetch task panicked before set_metadata");
    });
    assert!(worker.join().is_err());

    let waiter = Arc::clone(&index);
    let v2 = v.clone();
    let result = within_10s(move || waiter.wait_metadata("boom", &v2));
    assert!(result.fetch_error.as_deref().unwrap().contains("boom==1.0"));
}

/// The claim never overwrites a result the task already stored.
#[test]
fn claim_keeps_result_that_was_set() {
    let index = Arc::new(InMemoryIndex::new());
    let v: VypVersion = "1.0".parse().unwrap();
    assert!(index.register_metadata("fine", &v));
    {
        let _claim = MetadataClaim::new(Arc::clone(&index), "fine", &v);
        index.set_metadata(
            "fine",
            &v,
            MetadataResult {
                dependencies: Vec::new(),
                full_metadata: None,
                fetch_error: None,
            },
        );
    }
    assert!(index.wait_metadata("fine", &v).fetch_error.is_none());
}
