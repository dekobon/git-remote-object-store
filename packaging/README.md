# Packaging

Templates and metadata that the release pipeline
(`.github/workflows/release.yml`) renders into native package
artefacts on every `v*` tag.

| Format | Source | Built by |
|--------|--------|----------|
| `.deb` (Debian / Ubuntu) | `[package.metadata.deb]` in [`cli/Cargo.toml`](../cli/Cargo.toml) | `cargo-deb` in the `package-deb` job |
| `.rpm` (RHEL / Fedora / Amazon Linux) | `[package.metadata.generate-rpm]` in [`cli/Cargo.toml`](../cli/Cargo.toml) | `cargo-generate-rpm` in the `package-rpm` job |
| `.apk` (Alpine) | [`alpine/APKBUILD.in`](alpine/APKBUILD.in) | `abuild` inside an `alpine:3.20` Docker container in the `package-apk` job |
| Homebrew formula | [`homebrew/git-remote-object-store.rb.tmpl`](homebrew/git-remote-object-store.rb.tmpl) | `sed`-rendered and pushed to the tap repo in `publish` |

Binary archives (`.tar.gz` / `.zip`) are built directly by the
`build` job and need no template here.

The Homebrew formula targets Apple Silicon only, matching the
single macOS release target (`aarch64-apple-darwin`). Intel users
can `cargo install git-remote-object-store-cli` or run the aarch64
binary under Rosetta 2.

See [`docs/development/cutting-a-release.md`](../docs/development/cutting-a-release.md)
for the end-to-end release runbook.
