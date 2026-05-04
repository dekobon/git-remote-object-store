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

use sha2::{Digest, Sha256};

/// Body size for multipart-upload integration tests: just above the
/// 64 MiB `MULTIPART_PUT_THRESHOLD` so the dispatch picks multipart
/// without paying ~6 GiB of disk for a separate 5 GiB regression test
/// (which is gated on `RUN_LARGE_BODY_TESTS=1`).
pub const MULTIPART_TEST_SIZE: usize = 80 * 1024 * 1024;

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
