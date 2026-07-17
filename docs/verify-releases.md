# Verifying holofs releases (P2.3)

Every tagged release since v1.0.1 ships with **sigstore keyless
signatures** plus a **CycloneDX SBOM** covering every workspace crate.
This page is the operator's checklist for verifying an artifact before
you deploy it — the whole point of P2.3 is that "the tarball is on
GitHub" is not the same as "the tarball is what the CI built".

The signing identity is the GitHub Actions workflow
`.github/workflows/release.yml` running on a semver tag ref
(`refs/tags/v*.*.*`); anything signed by a different identity is a red
flag.

Cosign versions ≥ 2.4 all work; the release job pins to `v2.4.1` so
your local cosign only needs to match the underlying sigstore protocol
(compatible across 2.x). Install locally:

```sh
# macOS
brew install cosign
# Linux (curl)
curl -sSLO \
  https://github.com/sigstore/cosign/releases/download/v2.4.1/cosign-linux-amd64
chmod +x cosign-linux-amd64 && sudo mv cosign-linux-amd64 /usr/local/bin/cosign
```

## Binary tarballs

Every `holofs-<target>.tar.gz` release asset has a matching
`.sig` (detached signature) and `.pem` (Fulcio-issued signing
certificate) side by side. Download all three:

```sh
V=v1.2.3  # replace with the release you're verifying
T=x86_64-unknown-linux-gnu
for ext in tar.gz tar.gz.sig tar.gz.pem; do
  curl -sSLO "https://github.com/holofs/holofs/releases/download/${V}/holofs-${T}.${ext}"
done
```

Verify:

```sh
cosign verify-blob \
  --certificate holofs-${T}.tar.gz.pem \
  --signature   holofs-${T}.tar.gz.sig \
  --certificate-identity-regexp 'https://github.com/.*/holofs/.github/workflows/release.yml@refs/tags/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  holofs-${T}.tar.gz
```

Expected output ends with `Verified OK`. Failure modes to treat as
compromised:

* **`Fulcio certificate identity does not match`** — someone re-signed
  the artifact from a workflow that isn't `release.yml` on a version
  tag. Do not deploy.
* **`transparency log entry not found`** — sigstore's Rekor log has no
  entry for this signature. Either the tag ref was force-pushed after
  release, or the artifact was signed offline outside the CI path.
  Do not deploy.
* **hash mismatch** — the tarball bytes differ from what was signed.
  Do not deploy; report to the repo maintainers.

## Container images

The multi-arch OCI manifest is signed under the same identity as the
tarballs. Verify:

```sh
cosign verify \
  --certificate-identity-regexp 'https://github.com/.*/holofs/.github/workflows/release.yml@refs/tags/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  ghcr.io/holofs/holofs:v1.2.3
```

The output includes the digest cosign checked (`Verified OK` +
`{"critical":{"identity":...,"image":{"docker-manifest-digest":"sha256:..."}}}`).
Feed that digest to your image puller (Kubernetes `image:
ghcr.io/holofs/holofs@sha256:...`) to close the tag-vs-digest race:
even if `:v1.2.3` is later re-tagged, your pods still resolve the
signed bytes.

## SBOM (CycloneDX)

`holofs-sbom.tar.gz` is a per-crate CycloneDX 1.4 JSON bundle — one
`<crate>.cdx.json` per workspace member. Consume with the standard
sigstore/OWASP tooling:

```sh
# CVE scan with grype (Anchore)
tar -xzf holofs-sbom.tar.gz -C sbom/
grype sbom:./sbom/holofs-web.cdx.json

# CVE scan with trivy (Aqua)
trivy sbom ./sbom/holofs-web.cdx.json

# Feed into Dependency-Track for org-wide policy
dtrackauditor -a upload -p <project-uuid> -f sbom/holofs-web.cdx.json
```

Compared against a self-generated
`cargo cyclonedx --all --format json` on the source tarball, the two
must produce byte-identical `component` lists — the shipped SBOM is
generated from the same `Cargo.lock` the CI built. A diff there
suggests the release binary was built from a lockfile you don't have
sources for.

## Threat model

The signatures gate **who** produced the binary (must be the GH
Actions workflow on a version tag) and **whether the bytes match what
was signed**. They do NOT gate:

* Bugs / vulnerabilities in the code itself — that's what the SBOM +
  `grype` / `trivy` are for on the operator side, plus `cargo audit`
  in the CI job.
* Trust in GitHub Actions infrastructure. A GH compromise could
  theoretically issue a Fulcio cert to a malicious workflow. Rekor
  transparency log makes this detectable but not preventable —
  operators running high-value clusters should also compare their
  observed signer identity against a known-good baseline from a
  previous release.

## When we can't sign (breakglass)

Local `cargo build` outputs and CI runs on non-tag branches (`main`,
`develop`) don't produce signatures — the release workflow only fires
on `push: tags: ["v*.*.*"]`. Deploying those in production is
explicitly unsupported: use a proper tagged release. If the sigstore
public-good infrastructure is down (rare — historic outages are on the
order of hours), the release workflow's cosign step will fail loud
rather than silently unsign, so no unsigned artifact reaches the
release. In that case tag the release and re-run the workflow when
sigstore is back.
