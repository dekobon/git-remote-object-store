---
globs: "**/*"
alwaysApply: false
---

## Build Before Test

Always rebuild (`cargo build`) before running integration tests. Never test against a stale binary.

## No Incidental Coupling in Test Infrastructure

Before encoding any property into test code, verify that the **system under test** actually depends on it. If the code doesn't branch on a property, the tests must not couple to it. Prefer runtime discovery (scanning, globbing) over constructing exact values from environmental or structural assumptions.

Common traps:

- **Host environment** (`cfg(target_arch)`, `std::env::consts::ARCH/OS`): only use if the code under test is architecture/OS-specific
- **Filename structure**: don't parse and reconstruct every segment of a naming convention — couple only to the segments the code needs
- **Directory layout**: don't hardcode paths that reflect organizational choices irrelevant to the logic being tested

## Large-body integration tests (`RUN_LARGE_BODY_TESTS`)

A handful of integration tests upload a body in the > 5 GiB class to
exercise failure modes (S3 single-PUT ceiling, multipart-copy ceiling,
Azure block-count regimes) that the cheap 80 MiB tests cannot reach.
They are gated behind both `#[ignore]` and the `RUN_LARGE_BODY_TESTS`
env var so a default `cargo test` never pays for them.

Local run command:

```bash
RUN_LARGE_BODY_TESTS=1 cargo test --features integration-s3 -- \
    --ignored multipart_put_path_above_5_gib_round_trips
RUN_LARGE_BODY_TESTS=1 cargo test --features integration-azure -- \
    --ignored multipart_put_path_above_5_gib_round_trips
```

Cost per run, per backend:

- **Disk**: ~12 GiB scratch (the source plus a downloaded copy for
  round-trip hashing). The tests use `tempfile::tempdir()`, which
  honors `TMPDIR`; redirect to a roomier filesystem if `/tmp` is small.
- **Time**: dominated by container-startup and the upload/download
  loop. On a developer laptop against the in-Docker emulators this is
  several minutes; against real S3 / Azure it depends on egress
  bandwidth.
- **Egress**: zero against the local containers (the integration
  fixtures use `RustFS` for S3 and Azurite for Azure); against a real
  cloud backend, ~12 GiB of data transfer per run.

The mid-body abort tests (`multipart_put_path_aborts_on_midbody_truncation`)
are NOT gated — they run as part of the normal `integration-s3` /
`integration-azure` suites because they only need the 80 MiB body
the rest of the multipart suite already costs.
