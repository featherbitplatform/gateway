# Design: headless image variant + `admin.ui_enabled`

**Date:** 2026-07-26
**Branch:** `feature/headless-image`
**Status:** approved

## Problem

The gateway always embeds the admin web UI (rust-embed of `ui/dist/` at compile
time) and always serves it. Operators who run the gateway headless (API-only,
locked-down environments) want an image without the UI bits, and full-image
users want a switch to turn the UI off. Docker Hub should carry both variants.

## Decisions

| Decision | Choice |
|---|---|
| No-UI mechanism | Default-on cargo feature `ui` (compile-time) **plus** runtime `admin.ui_enabled` (default `true`) |
| Variant naming | Same Hub repo, `-headless` tag suffix: `latest-headless`, `edge-headless`, `X.Y.Z-headless`, `X.Y-headless` |
| Docker build | Single Dockerfile with `ARG CARGO_FLAGS`; headless legs pass `--no-default-features` |
| UPX | **Rejected** (page-sharing loss, AV/scanner opacity outweigh size win at 26 MB) |

## 1. Cargo feature `ui`

- `Cargo.toml`: `[features]` with `default = ["ui"]`,
  `ui = ["dep:rust-embed", "dep:mime_guess"]`; `rust-embed` and `mime_guess`
  become `optional = true`.
- `src/admin/ui.rs` and its `mod` declaration are gated `#[cfg(feature = "ui")]`.
- Headless build: `cargo build --release --no-default-features` — no embedded
  assets, no rust-embed code path, `ui/dist/` need not exist.
- The full (default-feature) build is byte-for-byte unaffected in behavior.

## 2. Runtime setting `admin.ui_enabled`

- `AdminConfig` (`src/config/system.rs`): new `ui_enabled: bool`,
  `#[serde(default = "default_true")]` — absent key keeps today's behavior.
- Admin router construction mounts the SPA fallback only when
  `cfg!(feature = "ui")` **and** `ui_enabled`. Otherwise non-API admin paths
  return plain `404 Not Found`.
- Restart-gated like all of `system.yaml`; in a headless binary the setting is
  inert (documented, no warning).
- `config/system.yaml` example gains a commented `ui_enabled: true` line.

## 3. Dockerfile

- `ARG CARGO_FLAGS=""` in the builder stage; build command becomes
  `cargo build --release ${CARGO_FLAGS}`.
- Full variant: unchanged flow (CI builds `ui/dist` first).
- Headless variant: CI runs `mkdir -p ui/dist` (satisfies the existing `COPY`,
  embeds nothing since the feature is off) and passes
  `CARGO_FLAGS=--no-default-features`.
- Final stage unchanged (`scratch`, binary, config, uid 65532): variants differ
  only in the binary.

## 4. Workflow `docker.yml`

- `build` matrix: platform × variant →
  `{linux/amd64, linux/arm64} × {default, headless}` on native runners.
  Headless legs skip the Node/npm steps entirely. Digest artifacts:
  `digests-<variant>-<platform-slug>`; GHA cache scope per (variant, platform).
- `merge` matrix over variant. Default variant: today's tag rules unchanged.
  Headless variant: same trigger logic with explicit suffixed tags —
  `X.Y.Z-headless`, `X.Y-headless`, `latest-headless` (main push and release
  tags), `edge-headless` (develop push). Explicit per-variant tag rules, not
  `flavor:` suffix magic.
- Empty-tags guard, concurrency group, permissions, readme-sync: unchanged
  (single Hub repo).

## 5. CI guard

- `ci.yml`: new fast step/job `cargo check --no-default-features` (no UI build
  required) so the headless combination cannot silently rot.

## 6. Testing

- Config: `ui_enabled` absent → `true`; explicit `false` parses.
- Admin router (full build): integration test — non-API admin path serves the
  SPA (`index.html`) when enabled; returns `404` when `ui_enabled: false`;
  admin API endpoints unaffected in both modes.
- Headless compile is covered by the CI `cargo check --no-default-features`.
- Existing unit/e2e suites keep running against the full build unchanged.

## 7. Documentation

- `DOCKERHUB.md`: `-headless` rows in the Tags table + a short "Headless
  variant" paragraph (no web editor; admin REST API fully functional).
- Website admin/UI reference page: document `admin.ui_enabled` (restart-gated;
  inert in headless builds).
- `CLAUDE.md`: one line about the `ui` feature + headless image variant.

## Out of scope

- UPX compression (explicitly rejected).
- Separate Hub repository for the headless variant.
- Scanning the headless image in `security.yml` (full image remains the scan
  target).
- Python runtime, `unpack` node, and other unrelated follow-ups.
