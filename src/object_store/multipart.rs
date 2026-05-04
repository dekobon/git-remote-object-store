//! Backend-neutral multipart-upload constants and part planner.
//!
//! Both the S3 and Azure backends use the same threshold and part-size
//! defaults so the decision to switch from a single-shot put to a
//! multipart upload is identical regardless of which backend the user
//! has configured. S3 *requires* multipart above a 5 GiB single-PUT
//! ceiling; Azure does not technically require it, but a single
//! `BlockBlobClient::upload` call for a multi-GiB body is opaque and
//! error-prone — explicit `stage_block` + `commit_block_list` gives us
//! per-block retries, predictable concurrency, and per-block progress
//! events. Issue #53.
//!
//! Below [`MULTIPART_PUT_THRESHOLD`] both backends keep their existing
//! single-call paths so small bundles, lock files, and HEAD writes do
//! not pay the `CreateMultipartUpload` round-trip cost.

use std::os::unix::fs::FileExt;
use std::sync::Arc;

use bytes::Bytes;

use super::ObjectStoreError;
use super::error::other_boxed;

/// Object size at or above which uploads switch from a single PUT/upload
/// call to explicit multipart. Same value for S3 and Azure: this fixes
/// the S3 5 GiB ceiling and gives Azure per-block control on large
/// transfers (issue #53).
///
/// Chosen to be small enough that integration tests can exercise the
/// multipart path with modestly sized synthetic bodies, and large
/// enough that ordinary bundle / lock / HEAD writes never pay the
/// `CreateMultipartUpload` round trip.
pub(crate) const MULTIPART_PUT_THRESHOLD: u64 = 64 * 1024 * 1024;

/// Default per-part size. 16 MiB satisfies S3's 5 MiB minimum (S3
/// rejects any non-final part below 5 MiB, except the last one) and
/// yields ≤ 10 000 parts up to ~156 GiB; for larger objects
/// [`plan_upload_parts`] scales the part size up.
pub(crate) const MULTIPART_PUT_PART_SIZE: u64 = 16 * 1024 * 1024;

/// Cap on simultaneous in-flight part uploads. Matches the existing
/// download multipart concurrency in `s3::MULTIPART_MAX_CONCURRENCY`
/// so peak FD / socket / memory usage stays predictable across
/// upload and download.
pub(crate) const MULTIPART_PUT_MAX_CONCURRENCY: usize = 8;

/// S3's protocol cap on parts per multipart upload. AWS rejects
/// `CompleteMultipartUpload` with > 10 000 parts.
pub(crate) const S3_MAX_PARTS: u64 = 10_000;

/// Azure's protocol cap on blocks per blob. The SDK rejects
/// `CommitBlockList` with > 50 000 blocks.
pub(crate) const AZURE_MAX_BLOCKS: u64 = 50_000;

/// One slice of a multipart upload: zero-indexed offset into the
/// source body, and length in bytes. `length` is always non-zero;
/// the planner returns an empty Vec for `size == 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadPart {
    pub offset: u64,
    pub length: u64,
}

/// Returns `true` if a body of `size` should use multipart upload.
///
/// Pinned in tripwire tests on each backend so a future regression
/// that re-introduces a bare single-PUT for sizes above the threshold
/// is caught at compile/test time. Issue #53.
pub(crate) fn should_use_multipart(size: u64) -> bool {
    size >= MULTIPART_PUT_THRESHOLD
}

/// Plan part offsets/lengths for a multipart upload.
///
/// Returns at most `max_parts` non-empty parts whose lengths sum to
/// `size`. `target_part_size` is scaled up by powers of two until the
/// part count fits under `max_parts`; this matches the AWS S3
/// transfer manager's planner shape and satisfies S3's "no part
/// smaller than 5 MiB except the last" constraint as long as the
/// caller supplies `target_part_size >= 5 MiB`.
///
/// Returns an empty Vec for `size == 0` — caller must short-circuit
/// (a zero-byte multipart upload would create a useless `upload_id`
/// with no parts, and S3 rejects `CompleteMultipartUpload` with no
/// parts).
pub(crate) fn plan_upload_parts(
    size: u64,
    target_part_size: u64,
    max_parts: u64,
) -> Vec<UploadPart> {
    if size == 0 || target_part_size == 0 || max_parts == 0 {
        return Vec::new();
    }

    let part_size = scale_part_size(size, target_part_size, max_parts);
    let full_parts = size / part_size;
    let last_part = size % part_size;
    // `with_capacity` is best-effort; saturating to `usize::MAX` on a
    // 32-bit target is fine — the Vec will simply grow as needed.
    let part_count = usize::try_from(full_parts).unwrap_or(usize::MAX) + usize::from(last_part > 0);
    let mut parts = Vec::with_capacity(part_count);
    for i in 0..full_parts {
        parts.push(UploadPart {
            offset: i * part_size,
            length: part_size,
        });
    }
    if last_part > 0 {
        parts.push(UploadPart {
            offset: full_parts * part_size,
            length: last_part,
        });
    }
    parts
}

/// Read `part.length` bytes starting at `part.offset` from `file`
/// into a freshly-allocated `Bytes`, using a positional read so
/// concurrent tasks sharing the same file handle do not trample
/// each other's offsets.
///
/// Per-task `try_clone` would *not* work: stdlib's `File::try_clone`
/// (and therefore `tokio::fs::File::try_clone`) returns a new
/// `File` that references the same kernel open file description —
/// which holds the seek offset, so concurrent seeks via different
/// `File` handles trample. `read_exact_at` (`pread64`) bypasses
/// the offset entirely; it's thread-safe and kernel-defined to be
/// a no-op on the file's seek offset.
///
/// Sharing one open file description across all tasks instead of
/// re-opening by path closes the metadata/upload race: every task
/// sees the same inode regardless of concurrent rename or unlink
/// at the original path.
pub(crate) async fn read_file_part(
    file: Arc<std::fs::File>,
    part: UploadPart,
) -> Result<Bytes, ObjectStoreError> {
    let length = usize::try_from(part.length).map_err(other_boxed)?;
    // `read_exact_at` is blocking; offload to the blocking pool so
    // we don't stall the runtime for 16 MiB syscalls.
    let buf = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0u8; length];
        file.read_exact_at(&mut buf, part.offset)?;
        Ok(buf)
    })
    .await
    .map_err(other_boxed)?
    .map_err(other_boxed)?;
    Ok(Bytes::from(buf))
}

/// Zero-copy slice of an in-memory body for a single part. The
/// part's offset/length are bounded by the body size at the call
/// site, so the `usize::try_from` conversions cannot fail on a
/// target where the body itself fit.
pub(crate) fn slice_bytes_part(body: &Bytes, part: UploadPart) -> Result<Bytes, ObjectStoreError> {
    let offset = usize::try_from(part.offset).map_err(other_boxed)?;
    let length = usize::try_from(part.length).map_err(other_boxed)?;
    Ok(body.slice(offset..offset + length))
}

/// Compute the smallest power-of-two multiple of `target_part_size`
/// that yields a plan with `<= max_parts` parts.
fn scale_part_size(size: u64, target_part_size: u64, max_parts: u64) -> u64 {
    let mut part_size = target_part_size;
    while size.div_ceil(part_size) > max_parts {
        // Saturating shift in case a pathologically large size /
        // small max_parts pair would overflow; in practice
        // S3_MAX_PARTS=10_000 with target_part_size=16 MiB caps at
        // ~156 GiB before a single doubling kicks in, and the upper
        // multipart limit is 5 TiB.
        let next = part_size.checked_mul(2);
        match next {
            Some(n) => part_size = n,
            None => return part_size,
        }
    }
    part_size
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;

    #[test]
    fn should_use_multipart_below_threshold_is_false() {
        assert!(!should_use_multipart(0));
        assert!(!should_use_multipart(MULTIPART_PUT_THRESHOLD - 1));
    }

    #[test]
    fn should_use_multipart_at_or_above_threshold_is_true() {
        assert!(should_use_multipart(MULTIPART_PUT_THRESHOLD));
        assert!(should_use_multipart(MULTIPART_PUT_THRESHOLD + 1));
    }

    #[test]
    fn plan_upload_parts_zero_size_is_empty() {
        let parts = plan_upload_parts(0, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS);
        assert!(parts.is_empty());
    }

    #[test]
    fn plan_upload_parts_zero_target_part_size_is_empty() {
        let parts = plan_upload_parts(MULTIPART_PUT_PART_SIZE, 0, S3_MAX_PARTS);
        assert!(parts.is_empty());
    }

    #[test]
    fn plan_upload_parts_zero_max_parts_is_empty() {
        let parts = plan_upload_parts(MULTIPART_PUT_PART_SIZE, MULTIPART_PUT_PART_SIZE, 0);
        assert!(parts.is_empty());
    }

    #[test]
    fn plan_upload_parts_one_part_when_size_eq_part_size() {
        let parts = plan_upload_parts(16 * MIB, 16 * MIB, S3_MAX_PARTS);
        assert_eq!(
            parts,
            vec![UploadPart {
                offset: 0,
                length: 16 * MIB
            }]
        );
    }

    #[test]
    fn plan_upload_parts_last_part_short() {
        let parts = plan_upload_parts(16 * MIB + 1, 16 * MIB, S3_MAX_PARTS);
        assert_eq!(
            parts,
            vec![
                UploadPart {
                    offset: 0,
                    length: 16 * MIB,
                },
                UploadPart {
                    offset: 16 * MIB,
                    length: 1,
                },
            ]
        );
    }

    #[test]
    fn plan_upload_parts_threshold_boundary_yields_expected_part_count() {
        // 64 MiB at 16 MiB part size = exactly 4 parts.
        let parts = plan_upload_parts(
            MULTIPART_PUT_THRESHOLD,
            MULTIPART_PUT_PART_SIZE,
            S3_MAX_PARTS,
        );
        assert_eq!(parts.len(), 4);
        let total: u64 = parts.iter().map(|p| p.length).sum();
        assert_eq!(total, MULTIPART_PUT_THRESHOLD);
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p.offset, (i as u64) * MULTIPART_PUT_PART_SIZE);
            assert_eq!(p.length, MULTIPART_PUT_PART_SIZE);
        }
    }

    #[test]
    fn plan_upload_parts_lengths_sum_to_size() {
        // Verifies the planner is total: no bytes lost, none doubled.
        let cases = [
            1_u64,
            MULTIPART_PUT_PART_SIZE - 1,
            MULTIPART_PUT_PART_SIZE,
            MULTIPART_PUT_PART_SIZE + 1,
            7 * MULTIPART_PUT_PART_SIZE + 17,
            123 * MIB + 4567,
        ];
        for size in cases {
            let parts = plan_upload_parts(size, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS);
            let total: u64 = parts.iter().map(|p| p.length).sum();
            assert_eq!(total, size, "size={size}");
            // Offsets are contiguous and sorted.
            let mut expected_offset = 0_u64;
            for p in &parts {
                assert_eq!(p.offset, expected_offset, "size={size}");
                assert!(p.length > 0);
                expected_offset += p.length;
            }
        }
    }

    #[test]
    fn plan_upload_parts_scales_part_size_when_max_parts_exceeded() {
        // 200 GiB at 16 MiB target part size with S3's 10 000 cap:
        // 200 GiB / 16 MiB = 12 800 parts > 10 000, so the planner
        // doubles the part size to 32 MiB → 6 400 parts ≤ 10 000.
        let size = 200 * 1024 * MIB;
        let parts = plan_upload_parts(size, MULTIPART_PUT_PART_SIZE, S3_MAX_PARTS);
        assert!(
            (parts.len() as u64) <= S3_MAX_PARTS,
            "parts.len()={} > S3_MAX_PARTS={S3_MAX_PARTS}",
            parts.len(),
        );
        let total: u64 = parts.iter().map(|p| p.length).sum();
        assert_eq!(total, size);
        // Every full part should be 32 MiB (one power-of-two doubling).
        let expected_part_size = 32 * MIB;
        for p in parts.iter().take(parts.len() - 1) {
            assert_eq!(p.length, expected_part_size);
        }
    }

    #[test]
    fn plan_upload_parts_azure_block_cap_well_above_s3() {
        // Sanity: Azure's 50 000-block cap is reached only at much
        // larger sizes than S3's 10 000-part cap.
        let size = 200 * 1024 * MIB;
        let parts = plan_upload_parts(size, MULTIPART_PUT_PART_SIZE, AZURE_MAX_BLOCKS);
        // 200 GiB / 16 MiB = 12 800 — fits without scaling.
        assert!((parts.len() as u64) <= AZURE_MAX_BLOCKS);
        for p in parts.iter().take(parts.len() - 1) {
            assert_eq!(p.length, MULTIPART_PUT_PART_SIZE);
        }
    }
}
