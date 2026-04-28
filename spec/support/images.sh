# shellcheck shell=bash

# Image pin constants. Source of truth for the docker tags used by the
# shellspec integration suite. Must match the constants in
# `tests/s3_store_integration.rs` and `tests/azure_store_integration.rs`
# so a single backend bug reproduces in both Rust and shellspec layers.
# `make _ci-image-pin-check` enforces parity at CI time.

# shellcheck disable=SC2034 # consumed by sourcing helpers
RUSTFS_IMAGE="rustfs/rustfs"
# shellcheck disable=SC2034
RUSTFS_TAG="1.0.0-alpha.99"

# shellcheck disable=SC2034
AZURITE_IMAGE="mcr.microsoft.com/azure-storage/azurite"
# shellcheck disable=SC2034
AZURITE_TAG="3.35.0"
