# Security Policy

## Reporting a Vulnerability

If you believe you have found a security vulnerability in this crate,
please report it privately. **Do not open a public GitHub issue.**

Please report vulnerabilities via one of the following:

- **GitHub Security Advisories** (preferred): Use the
  ["Report a vulnerability"](https://github.com/dekobon/git-remote-object-store/security/advisories/new)
  button on this repository.
- **Email**: <e.zupancic@f5.com>

Please include the following in your report:

- A description of the vulnerability and its impact.
- Steps to reproduce, or a proof-of-concept.
- The affected version(s).
- Any suggested mitigation, if known.

## Response Expectations

- We will acknowledge receipt within **3 business days**.
- We aim to provide an initial assessment within **7 business days**.
- We will keep you informed of progress toward a fix and coordinate
  a disclosure timeline with you.
- Typical time-to-fix for confirmed vulnerabilities is **30–90 days**
  depending on severity and complexity.

## Disclosure Policy

We follow **coordinated disclosure**. Once a fix is available:

1. We will publish a patched release on crates.io and to GitHub
   Releases.
2. We will publish a [RustSec advisory](https://rustsec.org/) with a
   CVE identifier where appropriate.
3. We will credit the reporter in the advisory unless they prefer
   to remain anonymous.

We ask that reporters give us a reasonable window (typically 90 days)
to release a fix before public disclosure.

## Verifying release artefacts

Every `v*` tag publishes signed release artefacts to
[GitHub Releases](https://github.com/dekobon/git-remote-object-store/releases).

- **`SHA256SUMS`** — SHA-256 hashes of every artefact in the release.
- **`SHA256SUMS.minisig`** — [minisign](https://jedisct1.github.io/minisign/)
  signature over `SHA256SUMS`. Verify with the committed
  [`minisign.pub`](minisign.pub):

  ```bash
  minisign -Vm SHA256SUMS -p minisign.pub
  grep <artefact> SHA256SUMS | sha256sum -c
  ```

- **SLSA build provenance** — every binary archive and native package
  has a GitHub-signed provenance attestation. Verify with the `gh`
  CLI:

  ```bash
  gh attestation verify <artefact> -R dekobon/git-remote-object-store
  ```

- **CycloneDX SBOM** — `*.cdx.json` is published for both the library
  (`git-remote-object-store`) and the CLI (`git-remote-object-store-cli`).

If either signature check fails, do **not** install the artefact —
file a security report via the channels above.

## Scope

This policy covers vulnerabilities in the code of this crate itself.
Vulnerabilities in dependencies should be reported to the respective
upstream projects; we will update our dependency requirements promptly
once upstream fixes are available.

## Safe Harbor

We consider security research conducted in good faith under this policy
to be authorized. We will not pursue legal action against researchers
who:

- Make a good-faith effort to avoid privacy violations, data destruction,
  or service disruption.
- Report vulnerabilities promptly.
- Do not exploit the vulnerability beyond what is necessary to demonstrate it.
- Give us reasonable time to respond before public disclosure.
