# Security audit

This is a technical self-assessment pipeline, not a certification of the
application or of regulatory compliance. It does not change the build or
release workflows; it adds evidence next to them.

## Architecture

`scripts/security-audit.sh` is the local entry point. Its Python orchestrator
(`scripts/security_audit.py`) runs each installed tool without a shell,
captures stdout/stderr and exit codes, and writes
`security/reports/security-audit.json` plus a Markdown summary after every
check. `PASS`, `FAIL`, `WARNING`, `SKIPPED` and `ERROR` remain distinct. An
absent tool or malformed output is `ERROR`, never `PASS`. Generated evidence is
ignored by Git and should be reviewed before sharing.

The dedicated [workflow](../.github/workflows/security-audit.yml) runs on PRs
to `main`, pushes to `main`, Monday schedules and manual dispatch. Separate
Linux jobs run the dependency audit, policy checks, static analysis, unsafe
scan, secret scan, SBOM and security tests. Fuzz smoke tests run on schedules
and manual dispatch only. Every job uploads its report even if the check
fails, and an always-running final job combines them with
`scripts/security_aggregate.py`. A missing job artifact, or reports from
different commits, is `ERROR`.

## Local commands and tools

From the repository root:

```sh
./scripts/security-audit.sh all
./scripts/security-audit.sh dependencies licenses
./scripts/security-audit.sh static
./scripts/security-audit.sh unsafe
./scripts/security-audit.sh secrets
./scripts/security-audit.sh sbom
./scripts/security-audit.sh tests
./scripts/security-audit.sh fuzz
python3 -m unittest scripts.test_security_aggregate scripts.test_security_audit
```

Exit codes: `0` for PASS/WARNING, `1` for FAIL, `2` for ERROR. The script needs
Python 3, Rust/Cargo and the relevant tools in `PATH`: cargo-audit 0.23.0,
cargo-deny 0.20.2, cargo-geiger 0.14.0, cargo-cyclonedx 0.5.10, Gitleaks
8.30.1 and cargo-fuzz 0.13.2 with a nightly toolchain for fuzzing. The workflow
installs pinned tool versions; Gitleaks is downloaded with a checked SHA-256.
For a local scan with an already populated RustSec database, set
`SECURITY_AUDIT_OFFLINE=1`. `GITLEAKS_BIN` may point to a locally installed
binary. `SECURITY_FUZZ_TOOLCHAIN` selects the nightly toolchain; the workflow
uses `nightly-2026-11-11`.

## What each check covers

| Check | Tools | Blocks on |
| --- | --- | --- |
| `dependencies` | `cargo audit` on `Cargo.lock` and `fuzz/Cargo.lock` | critical/high/medium or unscored advisories |
| `licenses` | `cargo deny check advisories licenses sources bans` on both graphs | any policy error in `deny.toml` |
| `static` | `cargo fmt --check`; `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`; `forbid-unsafe-policy` | any formatting diff, lint, or crate not inheriting `unsafe_code = "forbid"` |
| `unsafe` | `cargo geiger` on `uvcraft-app`; lexical scan of own sources | tool error (inventory only) |
| `secrets` | `gitleaks git --redact` over the full history | any finding, or exceptions that are incomplete, expired or out of sync with `.gitleaksignore` |
| `sbom` | `cargo cyclonedx` per crate; `cargo metadata` for native `links` crates | generation failure |
| `tests` | `uvcraft-io` (parsers, external URIs), `uvcraft-commands` errors, `uvcraft-cli` RPC, `uvcraft-app` preferences | any failure |
| `fuzz` | `cargo fuzz` targets `obj_parse`, `gltf_parse`, `fbx_parse`, `command_script` | a crash, timeout or sanitizer report |

Clippy runs with `--all-features`, which enables `uvcraft-py`'s `python`
feature; the script sets `PYO3_BUILD_EXTENSION_MODULE=1` so this never links
libpython.

## Policies and exceptions

`deny.toml` checks RustSec advisories, the licenses present in the resolved
dependency graph, crates.io as the only registry, unknown Git sources and
wildcard/duplicate versions. Parallel versions are warnings because the
GUI/wgpu graph contains several upstream major versions. Local `uvcraft-*`
path dependencies appear as wildcards and are also warnings. Any new external
Git source or registry fails. The license allow list is a distribution policy
for `MIT OR Apache-2.0`, not proof of compatibility in every use; `NCSA` is
needed only by `libfuzzer-sys` in the never-shipped fuzz graph.

Critical, high and medium advisories block; advisories without a reliable
numerical CVSS score also block until reviewed. Only an explicit score is
classified (critical 9.0–10, high 7.0–8.9, medium 4.0–6.9, low 0.1–3.9,
informational 0); a vector or title is not a score. cargo-deny 0.20.2 treats
every vulnerability advisory as an error, so the combined CI stays blocked
until a finding is reviewed. Unmaintained-package notices are warnings.

Exceptions require an identifier, narrow scope, justification, owner, review
date and expiry in `security/exceptions.json`. There are none today, and
`.gitleaksignore` is empty. A Gitleaks exception must be an exact fingerprint
present in both files. A RustSec exception must be reviewed and represented in
both the policy and the report; a broad allowlist or a discarded failure is not
acceptable.

## Fuzzing

`fuzz/` is a separate cargo-fuzz crate (excluded from the workspace, with its
own audited lockfile). Inputs are capped at 64 KiB (16 KiB for scripts).
`gltf_parse` resolves external buffers against a directory that does not
exist, so only embedded data is read. Hand-written seeds live in `fuzz/seeds/`;
the generated corpus and crash artifacts are ignored by Git. AddressSanitizer
stays enabled; LeakSanitizer is disabled because some container ptrace
policies prevent it from finishing. Ten seconds per target is a smoke test;
deep fuzzing and corpus triage are manual follow-up work. The first run found a
panic on a glTF whose JSON root is not an object, now fixed and covered by a
unit test.

## Application hardening

- **Untrusted meshes.** External glTF buffers must be regular files inside
  the asset's folder: absolute paths, URI schemes, `..` segments (also
  percent-encoded), symlinks resolving outside the folder and device
  files/FIFOs are rejected, so a crafted `.gltf` cannot hang the loader on
  `/dev/zero` or pull an arbitrary file into a later `.glb` export. Assets
  that keep their buffers in a sibling folder must be moved next to them.
  Modified buffers are always written next to the output under generated
  names, and saves stage exclusive temporary files before renaming.
- **JSON-RPC server.** Request lines are capped at 64 MiB. The server has no
  authentication and, like the Blender bridge (`uvcraft bridge`, which also
  uses a random token and read timeouts), rejects non-loopback addresses
  through the shared `uvcraft_bridge::loopback_addr`; see `SECURITY.md`.
- **Preferences.** `preferences.json` (which stores selection sets keyed by
  mesh path, layouts and the texture path) is written through an exclusively
  created `0600` temporary file and never through a symbolic link.
  `Preferences::clear` erases it and stale temporaries; it is exposed as
  "Clear saved data" in the Preferences window and as
  `uvcraft-app --clear-preferences`.

## Evidence and coverage

cargo-cyclonedx creates one CycloneDX JSON per workspace crate; the
`uvcraft-app` SBOM is copied to `security/reports/sbom.cdx.json` and all crate
SBOMs stay under `sbom-components/`. They inventory Cargo packages, not system
libraries (Vulkan/GL drivers, libxkbcommon, Wayland/X11), the bundled icon or
Python. Gitleaks scans Git history with redaction and no persisted checkout
credentials; it does not see ignored or untracked files.

## Current baseline and maintenance

At introduction, RustSec reported no known vulnerability in either lockfile
and one unmaintained transitive crate, `paste` (RUSTSEC-2024-0436), which is
recorded as a warning. Formatting and strict Clippy findings that existed
before the pipeline were fixed in separate commits, so both checks block from
the start.

When updating dependencies or tools, review upstream release notes and
licenses, update the pinned version and checksum/SHA, rerun the local audit,
then inspect the CI artifacts. Third-party GitHub Actions in both workflows
are pinned to commit SHAs, checkout does not persist credentials, jobs use
read-only repository permission (the release job alone gets `contents:
write`), PR code receives no secrets, and release archives are built without a
shared build cache. Dependabot proposes Cargo (workspace and fuzz) and Actions
updates weekly.

Primary references: [RustSec cargo-audit](https://github.com/rustsec/rustsec/tree/main/cargo-audit),
[cargo-deny](https://embarkstudios.github.io/cargo-deny/),
[cargo-geiger](https://github.com/rust-secure-code/cargo-geiger),
[cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz),
[CycloneDX Rust](https://github.com/CycloneDX/cyclonedx-rust-cargo),
[Gitleaks](https://github.com/gitleaks/gitleaks).
