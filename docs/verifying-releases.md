# Verifying releases

Every `v*` tag publishes signed, attested artefacts to
[GitHub Releases](https://github.com/dekobon/git-remote-object-store/releases).
Each release ships:

- Per-target binary archives (Linux musl x86_64 / aarch64, etc.)
- A `SHA256SUMS` manifest covering every archive
- `SHA256SUMS.minisig` — a minisign signature over the manifest
- A SLSA build provenance attestation signed by the runner's GitHub
  OIDC identity
- CycloneDX SBOMs (`*.cdx.json`) for both the library and the CLI

## Verifying an archive

```bash
gh release download vX.Y.Z -p '*x86_64-unknown-linux-musl.tar.gz' \
                          -p SHA256SUMS -p SHA256SUMS.minisig
minisign -Vm SHA256SUMS -p minisign.pub
grep musl SHA256SUMS | sha256sum -c
gh attestation verify git-remote-object-store-X.Y.Z-x86_64-unknown-linux-musl.tar.gz \
                     -R dekobon/git-remote-object-store
```

`SHA256SUMS` is signed with [minisign](https://jedisct1.github.io/minisign/)
against the committed [`minisign.pub`](../minisign.pub) at the repository
root. The SLSA attestation is verified against the GitHub Actions
workflow that produced the artefact.

## Related documents

- [`docs/development/cutting-a-release.md`](development/cutting-a-release.md) — the
  release pipeline as run by maintainers.
- [`SECURITY.md`](../SECURITY.md) — vulnerability reporting flow.
