# SAST Pipeline — Design

**Date:** 2026-07-24
**Status:** Implemented

## Goal

One SAST pipeline, runnable identically in two places: locally on a developer
machine (Windows-first, via Docker) and in GitHub Actions. Sonar was
considered and dropped — it needs a server, and its linting role is covered
by clippy + rustfmt (Rust), ESLint (UI TypeScript), and semgrep (security
patterns).

## Principles

- **Shared configs are the source of truth.** Every scanner reads the same
  file at the repo root locally and in CI, so thresholds cannot drift.
- **Block on high/critical; report the rest.** Lower severities surface in
  scan output and the GitHub Security tab without turning builds red.
- **No silent suppression.** Every allowlist/ignore entry carries a comment
  explaining why it is accepted.

## Scanners and configs

| Scanner | Surface | Config | Blocking gate |
|---|---|---|---|
| semgrep | Rust/TS/JS/Dockerfile code patterns | `.semgrepignore` + rulesets `p/default p/rust p/typescript p/dockerfile` | ERROR-severity findings |
| cargo-deny | Cargo.lock: RustSec advisories, licenses, bans, sources | `deny.toml` | advisory errors, disallowed licenses |
| grype | Filesystem (all four lockfiles) + container image | `.grype.yaml` | high/critical CVEs |
| hadolint | `Dockerfile`, `ui/Dockerfile`, `dev/echo-backend/Dockerfile` | `.hadolint.yaml` | error-level rules (warnings reported only) |
| gitleaks | Working-tree secrets (`dir` mode — covers untracked files) | `.gitleaks.toml` | any non-allowlisted leak |
| npm audit | `ui/`, `e2e/`, `website/` lockfiles | n/a (threshold on CLI) | high/critical |
| trivy | Container image | `trivy.yaml` | high/critical |

cargo-audit was deliberately dropped: cargo-deny's `advisories` check queries
the same RustSec database and adds license/ban/source checks.

## Local pipeline

`dev/sast.ps1` (Windows) and `dev/sast.sh` (Linux/macOS twin) run each tool
via its official Docker image with the repo mounted read-only; cargo-deny runs
natively (no official image exists — `cargo install cargo-deny --locked`).
Targets: `semgrep deny grype hadolint gitleaks npm-audit image all` (default
`all` = everything except the image scan). Grype/trivy vulnerability DBs are
cached in named Docker volumes. The `image` target builds the Dockerfile,
`docker save`s the image, and scans the archive — scanners never get the
Docker socket. A summary table prints at the end; exit code is non-zero if
any scan failed.

## GitHub CI

`.github/workflows/security.yml`: parallel jobs (semgrep, cargo-deny,
grype-fs, hadolint, gitleaks, npm-audit ×3 matrix, image-scan with grype +
trivy), each uploading SARIF to the Security tab where the tool supports it.
Triggers: PRs, pushes to main, weekly cron (new CVEs get published without
code changes), manual dispatch. Actions pinned at their current majors
(cargo-deny-action@v2, scan-action@v7, hadolint-action@v3,
trivy-action@0.36.0); gitleaks runs via its container directly — same
invocation as the local script.

`ci.yml` additions: `cargo fmt --check` in the rust job (the tree was
reformatted once with default rustfmt before the gate landed); a blocking
`ui-lint` ESLint job (the tree is eslint-clean, warnings included).

## Baseline triage (done as part of implementation)

- `rustls-webpki` 0.103.11 → 0.103.13 (`cargo update`): fixes
  RUSTSEC-2026-0098/0099/0104.
- `prometheus` 0.13 → 0.14: drops vulnerable protobuf 2.x
  (RUSTSEC-2024-0437); one call-site fix in `src/server/listener.rs`; full
  test suite passes (738 tests).
- Ignored with comments in `deny.toml`: RUSTSEC-2024-0384 (`instant`
  unmaintained, via notify), RUSTSEC-2025-0134 (`rustls-pemfile`
  unmaintained) — no safe upgrades exist.
- `ui/`: `npm audit fix` cleared all findings (vite, postcss, fast-uri).
- `website/`: `npm audit fix` + an `overrides` pin of
  `serialize-javascript@^7.0.5` (RCE advisory, transitive via webpack
  plugins); Docusaurus build verified. 18 moderates remain, below threshold.
- Dockerfile hardening: `USER 65532:65532` in the gateway image,
  `USER nobody` in the echo backend (semgrep missing-user).
- `.gitignore`: `tests/pg-data-keycloak/` (live Postgres data dir),
  `sast-out/`. The ~82 MB vendored APISIX reference copy now lives at
  `plan/apisix` (`plan/` was already gitignored), and every scanner excludes
  `plan/`.
- `Cargo.toml`: `publish = false` (lets cargo-deny skip licensing the
  private crate).

## Warning cleanup (second pass — everything non-blocking addressed too)

- UI ESLint: all 4 errors + 3 warnings fixed with real refactors —
  `TraceViewer`/`GraphCanvas` reset-on-prop-change effects replaced with the
  canonical `key=` remount pattern, `formatDuration` moved to `src/format.ts`
  (react-refresh), initial fetch scheduled as a promise callback. Validated by
  the full Playwright e2e suite (95 scenarios). `ui-lint` is now blocking.
- Website npm audit: 0 vulnerabilities at every severity. The whole moderate
  tree was one root cause (`uuid` < 11.1.1 inside sockjs → webpack-dev-server
  → @docusaurus/core); fixed with a scoped override. Dev server and
  production build verified.
- cargo-deny: warning-free — unused `Unicode-DFS-2016` allowance removed;
  the 9 known duplicate-version crates (platform/ecosystem shims) moved to a
  documented `[bans].skip` list, so *new* duplicates still warn.
- hadolint DL3018: inline ignore on the builder-stage `apk add` with a
  comment (pins would break on every `rust:alpine` refresh).
- rustfmt: the original "fmt-clean" probe was a false pass (piped exit
  code); the tree was reformatted once with default rustfmt and the CI gate
  is real now. Tests re-run green after the reformat.

## Follow-ups

- `LICENSE` file (Apache-2.0) contradicts the README badge (MIT) — pick one.
- Consider pinning scanner image tags/digests in `dev/sast.*` once a cadence
  for bumping them exists.
