# Cutting a release

Step-by-step procedure for shipping `git-remote-object-store`. The
pipeline is defined in
[`.github/workflows/release.yml`](../../.github/workflows/release.yml).
Everything downstream of `git push --tags` is automated.

## What the release pipeline does

One push of a `v*` tag runs this end-to-end:

1. **preflight** — validates the tag, checks `Cargo.toml` version
   parity for **both** `git-remote-object-store` (library) and
   `git-remote-object-store-cli`, confirms `minisign.pub` is not the
   committed placeholder, runs `cargo publish --dry-run` for the
   library, extracts the matching `CHANGELOG.md` section as release
   notes.
2. **build** — cross-compiles the CLI for eight targets (Linux
   gnu/musl × x86_64/aarch64, FreeBSD x86_64, macOS aarch64, Windows
   x86_64/aarch64). Strips binaries, captures debug symbols
   (`.debug` / `.dSYM` / `.pdb`), runs `cargo about generate` against
   [`about.toml`](../../about.toml) /
   [`about.hbs`](../../about.hbs) to produce
   `THIRD-PARTY-LICENSES.md` per target, and produces per-target
   `.tar.gz` / `.zip` archives.
3. **package-{deb,rpm,apk}** — builds `.deb`, `.rpm`, and `.apk`
   artefacts from the staged binaries. (FreeBSD and macOS ship as
   tarballs only.)
4. **smoke-{deb,rpm,apk,macos,windows}** — installs each package
   inside the appropriate container/VM (Ubuntu 22.04/24.04, Debian
   12; Rocky 9, Fedora, Amazon Linux 2023; Alpine 3.20; macOS;
   Windows) and asserts `git-remote-object-store --version`
   matches the tag. The deb lane additionally enforces that the
   `+`-form symlinks (`git-remote-s3+https`, etc.) are present and
   that the bundled `THIRD-PARTY-LICENSES.md` contains the ring
   `Apache-2.0 AND ISC` license texts.
5. **sign-attest** — flattens every artefact into `release/`,
   generates CycloneDX SBOMs for both crates, computes
   `SHA256SUMS`, signs it with minisign, and attaches SLSA build
   provenance to every binary archive and native package.
6. **publish** — creates/updates the GitHub Release, attaches every
   artefact + `SHA256SUMS` + `SHA256SUMS.minisig`. For non
   pre-releases, also renders and pushes the Homebrew formula.
7. **publish-crates** — for non pre-releases, runs `cargo publish`
   for `git-remote-object-store` (library) then
   `git-remote-object-store-cli` (binaries) in order. Skips
   idempotently if the version is already on crates.io (so
   `workflow_dispatch` re-runs on the same tag don't error out on
   the duplicate upload). The preflight stage also runs `cargo
   publish --dry-run` for the library on every release —
   pre-releases included — so packaging errors surface before any
   external step.
8. **verify** — downloads the published `musl` tarball back out of
   the release, verifies the minisign signature, checksum, and
   SLSA provenance.

If any stage fails, nothing downstream runs. `publish` and
`publish-crates` are the only jobs that mutate anything outside this
repo; they run in parallel so a crates.io failure does not block the
GitHub Release's `verify` step (and vice versa).

## Prerequisites (one-time setup)

You only need to do this once per project, but verify each item
before your first real release.

### Repository secrets

Configure these under **Settings → Secrets and variables → Actions**:

| Secret                   | Purpose                                                            |
| ------------------------ | ------------------------------------------------------------------ |
| `MINISIGN_SECRET_KEY`    | minisign secret-key file contents (base64 body + comment lines) used to sign `SHA256SUMS` |
| `MINISIGN_PASSWORD`      | Password for that key                                              |
| `ALPINE_ABUILD_KEY_PRIV` | abuild RSA private key (Alpine `.apk` signing)                     |
| `ALPINE_ABUILD_KEY_PUB`  | Matching public key                                                |
| `HOMEBREW_TAP_TOKEN`     | Fine-grained PAT with write access to `dekobon/homebrew-git-remote-object-store` |

The Homebrew tap secret is optional — if it's missing, the publish
step warns and skips the tap push, leaving the GitHub Release
intact.

crates.io authentication does **not** use a repository secret; the
`publish-crates` job authenticates via
[Trusted Publishing](https://crates.io/docs/trusted-publishing).
See [crates.io Trusted Publisher setup](#cratesio-trusted-publisher-setup)
below.

### Generating the minisign keypair

```bash
minisign -G -p minisign.pub -s minisign.key
```

- Commit `minisign.pub` to the repo (replacing the placeholder).
- Paste the **contents** of `minisign.key` into the
  `MINISIGN_SECRET_KEY` repository secret.
- Paste the password into `MINISIGN_PASSWORD`.

The preflight step refuses to release while the placeholder is in
place — the repo can't accidentally publish unsigned artefacts.

### Generating the abuild keypair (Alpine)

```bash
docker run --rm -it -v "$PWD/abuild-keys:/k" alpine:3.20 sh -c '
  apk add --no-cache abuild
  cd /k
  abuild-keygen -a -n
'
sudo chown -R "$USER:$USER" abuild-keys
```

- Paste the `*.rsa` contents into `ALPINE_ABUILD_KEY_PRIV`.
- Paste the `*.rsa.pub` contents into `ALPINE_ABUILD_KEY_PUB`.
- The keypair is opaque to this repo; back it up off-machine if
  you want signed `.apk` artefacts to remain reproducible across
  key rotation.

### `release` GitHub Environment

The `publish-crates` job runs `environment: release`. Create the
environment under **Settings → Environments** before merging this
workflow — empty is fine; the existence of the environment is what
the OIDC `environment` claim binds to.

Add `main` (or your release branch) to "deployment branches" if
you want to lock release runs to a specific branch.

### crates.io Trusted Publisher setup

Both crates need a Trusted Publisher entry under
**crates.io → your crate → Trusted Publishing**:

| Field        | Value                                            |
| ------------ | ------------------------------------------------ |
| Owner        | `dekobon`                                        |
| Repository   | `git-remote-object-store`                        |
| Workflow     | `release.yml`                                    |
| Environment  | `release`                                        |

For the **first** publish of each crate you cannot register a TP
yet (crates.io requires the crate to exist), so do the first
publish via `cargo publish --token …` from a local checkout, then
register Trusted Publishing for subsequent releases. The pipeline's
idempotent skip logic (`curl … sparse-index`) means a re-run of the
same tag after manual first-publish is safe.

### Homebrew tap repo

Create an empty `dekobon/homebrew-git-remote-object-store`
repository, mint a fine-grained PAT scoped to **contents: write**
on it, and store as `HOMEBREW_TAP_TOKEN`. Without the secret, the
publish step skips the tap push without failing.

## Per-release procedure

1. **Update CHANGELOG.md.** Convert the `[Unreleased]` heading to
   the new version with the date:

   ```markdown
   ## [0.2.0] - 2026-05-15
   ```

   Add a fresh empty `[Unreleased]` section above it so future
   commits land somewhere.

2. **Bump versions in lockstep.** Edit **all three** sites:

   - `Cargo.toml` (root, library): `[package].version = "0.2.0"`
   - `cli/Cargo.toml`: `[package].version = "0.2.0"`
   - `cli/Cargo.toml`: under `[dependencies]`,
     `git-remote-object-store = { path = "..", version = "0.2.0" }`

   The third site is the version requirement applied to the library
   when the CLI is built from crates.io. A drift here passes every
   preceding stage and fails only at `publish-crates`, after the
   GitHub Release is already cut. The preflight step refuses to
   release if any of the three drifts from the tag.

3. **Run the local pre-flight:**

   ```bash
   make ci
   cargo publish -p git-remote-object-store --dry-run --locked
   cargo publish -p git-remote-object-store-cli --dry-run --locked --allow-dirty
   ```

4. **Commit and push:**

   ```bash
   git commit -am "release: 0.2.0"
   git push origin main
   ```

5. **Tag and push the tag:**

   ```bash
   git tag v0.2.0
   git push origin v0.2.0
   ```

6. **Watch the workflow** under **Actions → Release**. Every stage
   should turn green. Total wall-clock is dominated by the build
   matrix (~10–15 min on fresh runners; less when caches warm).

## Rehearsal via `workflow_dispatch`

Before the first real release, rehearse against a throwaway tag:

```bash
git tag v0.0.0-test1
git push origin v0.0.0-test1
```

Or trigger the workflow on an existing tag through the Actions UI
(`workflow_dispatch → tag = v0.0.0-test1`). Pre-release tags
(anything with a `-` suffix) skip the Homebrew tap push and
crates.io publish, so the rehearsal cannot leak. Clean up
afterwards:

```bash
gh release delete v0.0.0-test1 -y
git push origin :v0.0.0-test1
```

## Tag-format rules

- `vMAJOR.MINOR.PATCH` (e.g. `v0.2.0`) for releases.
- `vMAJOR.MINOR.PATCH-SUFFIX` for pre-releases (e.g.
  `v0.2.0-rc1`, `v0.2.0-alpha2`).
- **No dots inside the suffix** (e.g. `v0.2.0-rc.2` is rejected).
  Alpine's `abuild` does not accept dotted pre-release suffixes,
  and the preflight stage fails fast if you tag one — better than
  watching three apk lanes burn for two minutes each before
  exploding mid-build.

## Troubleshooting

- **`workflow_dispatch` checkout points at the wrong SHA.** The
  workflow always re-checks out the tag ref via
  `actions/checkout@…  with: ref: ${{ steps.tag.outputs.tag }}`.
  If you see drift, look for an in-flight job that started before
  the tag was force-moved.
- **`publish-crates` errored with "version already exists".** The
  job is idempotent; re-run on the same tag and it will skip the
  uploaded crate. If it errored on the *first* upload, you need to
  fix the underlying cause and either (a) re-run on the same tag,
  or (b) bump to the next patch version and tag again.
- **A smoke test failed because man pages were missing.** This
  release pipeline does not yet generate man pages — the smoke
  tests do not assert their presence. If you add an `xtask` to
  generate them, also add `man/*.1` to the archive staging step
  and the per-package metadata.

## Verifying release artefacts

Every release is signed and attested. Downstream verification:

```bash
gh release download vX.Y.Z -p '*x86_64-unknown-linux-musl.tar.gz' \
                          -p SHA256SUMS -p SHA256SUMS.minisig
minisign -Vm SHA256SUMS -p minisign.pub
grep musl SHA256SUMS | sha256sum -c
gh attestation verify git-remote-object-store-X.Y.Z-x86_64-unknown-linux-musl.tar.gz \
                     -R dekobon/git-remote-object-store
```

The `verify` job in the pipeline runs exactly this against the
published `musl` archive on every release, so a green pipeline is
already proof of end-to-end signature/attestation integrity.

## Public-flip checklist

Items that block on the repo being public on GitHub. None of the
in-repo configuration depends on them, but the pipeline cannot run
green until each is in place.

- [ ] Repo flipped to public on GitHub.
- [ ] `MINISIGN_SECRET_KEY` / `MINISIGN_PASSWORD` repo secrets set,
      and `minisign.pub` rotated from the placeholder.
- [ ] `ALPINE_ABUILD_KEY_PRIV` / `_PUB` repo secrets set (or
      accept ephemeral key warnings on every release).
- [ ] `release` GitHub Environment created.
- [ ] `dekobon/homebrew-git-remote-object-store` tap repo created
      and `HOMEBREW_TAP_TOKEN` PAT issued.
- [ ] First-publish of `git-remote-object-store` and
      `git-remote-object-store-cli` to crates.io done manually
      (the registry won't accept Trusted Publisher registration
      against an unpublished crate name).
- [ ] crates.io Trusted Publisher entries registered for both
      crates pointing at this repo's `release.yml` + `release`
      environment.
- [ ] Branch protection on `main`: require the aggregate `ci`
      check, require linear history, optionally require signed
      commits.
- [ ] Code Scanning enabled under **Settings → Security → Code
      security** so the CodeQL workflow's findings surface as
      alerts.
