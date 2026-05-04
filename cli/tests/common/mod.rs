//! Shared helpers for the per-backend integration test suites.
//!
//! Both `s3_store_integration.rs` and `azure_store_integration.rs`
//! exercise the multipart upload/download paths with the same
//! synthetic body shape and the same hashing recipe; this module
//! lives at `cli/tests/common/mod.rs` so each test crate can
//! `mod common;` and pull these in without duplicating them.
//!
//! `cargo test` treats each file under `tests/` as its own crate,
//! but it special-cases `tests/<name>/mod.rs` as a shared module
//! that does not become an integration-test crate of its own.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use git_remote_object_store::object_store::{ObjectStore, ProgressSink, PutOpts};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Body size for multipart-upload integration tests: just above the
/// 64 MiB `MULTIPART_PUT_THRESHOLD` so the dispatch picks multipart
/// without paying ~6 GiB of disk for a separate 5 GiB regression test
/// (which is gated on `RUN_LARGE_BODY_TESTS=1`).
pub const MULTIPART_TEST_SIZE: usize = 80 * 1024 * 1024;

/// Body size for the mid-body abort tests: 256 MiB / 16 parts at the
/// production 16 MiB part size. With the production
/// `MULTIPART_PUT_MAX_CONCURRENCY = 8`, half the parts are queued
/// behind the semaphore at upload start; their `pread`s only fire
/// after earlier parts' `upload_part`s release a permit, which gives
/// the truncate window time to land. A smaller body (e.g. 80 MiB / 5
/// parts) would dispatch all preads simultaneously and they would
/// often complete before the truncate.
pub const MIDBODY_ABORT_TEST_SIZE: usize = 256 * 1024 * 1024;

/// Body size for the >5 GiB multipart success regression: 6 GiB,
/// 1 GiB above the AWS single-PUT ceiling. Sized as `u64` because a
/// `usize` cannot universally hold > 4 GiB on 32-bit targets and these
/// tests stream the body to disk anyway (never materialised in RAM).
pub const LARGE_BODY_TEST_SIZE: u64 = 6 * 1024 * 1024 * 1024;

/// Chunk size used by [`write_repeating_pattern_file`] when materialising
/// a multi-GiB source on disk. 64 MiB is large enough that the per-write
/// syscall overhead is negligible at 6 GiB total, and small enough that
/// peak resident memory while building the file stays bounded.
pub const LARGE_BODY_CHUNK_SIZE: usize = 64 * 1024 * 1024;

/// Env var that gates the >5 GiB tests. Set to `1` (or any non-empty
/// value) to opt in.
pub const LARGE_BODY_ENV_VAR: &str = "RUN_LARGE_BODY_TESTS";

/// `true` if the >5 GiB body tests are enabled for this run.
pub fn large_body_tests_enabled() -> bool {
    std::env::var_os(LARGE_BODY_ENV_VAR).is_some_and(|v| !v.is_empty())
}

/// Build a deterministic byte buffer of `size` bytes. Each offset
/// gets a distinct byte (Knuth multiplicative-hash constant) so a
/// buffer reused across two parts would visibly corrupt the SHA256.
pub fn deterministic_payload(size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; size];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = u8::try_from(i.wrapping_mul(2_654_435_761) & 0xff).unwrap_or(0);
    }
    buf
}

/// SHA256 of an in-memory byte slice.
pub fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// Streaming SHA256 of a file on disk — used to verify a large
/// multipart download without double-buffering the body in process
/// memory.
pub fn sha256_of_file(path: &std::path::Path) -> [u8; 32] {
    use std::io::Read;
    let mut file = std::fs::File::open(path).expect("open file for hashing");
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hasher.finalize().into()
}

/// Spawn a background task that, after `delay`, truncates `path` to
/// zero bytes via a separate OS-level handle. The shared file handle
/// held by the multipart upload pipeline still references the same
/// inode, so its in-flight `pread`s past the new EOF return short and
/// surface as io errors. Used by the per-backend mid-body abort tests
/// to inject a deterministic part-read failure without coupling them
/// to the production constants.
pub fn spawn_truncator(
    path: std::path::PathBuf,
    delay: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .expect("reopen for truncate")
            .set_len(0)
            .await
            .expect("truncate src");
    })
}

/// Materialise a `total_size`-byte file by repeating `chunk` and
/// hash-as-we-write so the test never holds the full body in memory.
///
/// Returns the SHA256 of the on-disk body. The body is the byte
/// pattern `chunk` repeated to length; reordering or duplication of
/// any 16 MiB part during a multipart round-trip would corrupt the
/// SHA256 because the chunk repeats at a different period than the
/// part size.
pub async fn write_repeating_pattern_file(
    path: &std::path::Path,
    chunk: &[u8],
    total_size: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mut file = tokio::fs::File::create(path)
        .await
        .expect("create source file");
    let chunk_len_u64 = u64::try_from(chunk.len()).expect("chunk len fits in u64");
    let mut written: u64 = 0;
    while written < total_size {
        let remaining = total_size - written;
        let n = usize::try_from(remaining.min(chunk_len_u64)).expect("min fits in usize");
        hasher.update(&chunk[..n]);
        file.write_all(&chunk[..n]).await.expect("write chunk");
        written += u64::try_from(n).expect("usize fits in u64");
    }
    file.flush().await.expect("flush source file");
    hasher.finalize().into()
}

/// Drive `put_path` against `store` with a recording progress sink and
/// assert that at least two events fire and that their byte counts sum
/// to [`MULTIPART_TEST_SIZE`]. The body crosses
/// `MULTIPART_PUT_THRESHOLD`, so the dispatch picks the multipart path
/// and the sink should see one event per completed part / staged
/// block. Used by both per-backend integration suites to pin issue
/// #55's bundle-progress acceptance criterion. Sibling of
/// [`assert_put_bytes_emits_chunked_progress`]; the two helpers stay
/// parallel so each per-backend test reads as a two-liner that names
/// the variant under test.
pub async fn assert_put_path_emits_chunked_progress<S: ObjectStore + ?Sized>(store: &S, key: &str) {
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("progress-src.bin");
    tokio::fs::write(&src, &payload).await.expect("write src");

    let events: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let sink = ProgressSink::new(move |bytes| {
        recorded.lock().expect("progress lock").push(bytes);
    });

    store
        .put_path(
            key,
            &src,
            PutOpts {
                progress: Some(sink),
                ..PutOpts::default()
            },
        )
        .await
        .expect("multipart put_path with progress");

    let observed = events.lock().expect("progress lock").clone();
    assert!(
        observed.len() >= 2,
        "expected ≥ 2 progress events from put_path, got {observed:?}",
    );
    let total: u64 = observed.iter().sum();
    assert_eq!(
        total, MULTIPART_TEST_SIZE as u64,
        "put_path progress events must sum to the body size",
    );
}

/// Drive `put_bytes` against `store` with a recording progress sink and
/// assert that at least two events fire and that their byte counts sum
/// to [`MULTIPART_TEST_SIZE`]. The body crosses
/// `MULTIPART_PUT_THRESHOLD`, so the dispatch picks the multipart path
/// and the sink should see one event per completed part / staged
/// block. Sibling of [`assert_put_path_emits_chunked_progress`]; the
/// two helpers stay parallel so each per-backend test reads as a
/// two-liner that names the variant under test.
pub async fn assert_put_bytes_emits_chunked_progress<S: ObjectStore + ?Sized>(
    store: &S,
    key: &str,
) {
    let payload = deterministic_payload(MULTIPART_TEST_SIZE);

    let events: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&events);
    let sink = ProgressSink::new(move |bytes| {
        recorded.lock().expect("progress lock").push(bytes);
    });

    store
        .put_bytes(
            key,
            Bytes::from(payload),
            PutOpts {
                progress: Some(sink),
                ..PutOpts::default()
            },
        )
        .await
        .expect("multipart put_bytes with progress");

    let observed = events.lock().expect("progress lock").clone();
    assert!(
        observed.len() >= 2,
        "expected ≥ 2 progress events from put_bytes, got {observed:?}",
    );
    let total: u64 = observed.iter().sum();
    assert_eq!(
        total, MULTIPART_TEST_SIZE as u64,
        "put_bytes progress events must sum to the body size",
    );
}
