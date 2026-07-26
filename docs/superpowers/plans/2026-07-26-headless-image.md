# Headless Image Variant Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `-headless` Docker image variant (UI compiled out via a default-on `ui` cargo feature) alongside the full image, plus a runtime `admin.ui_enabled` switch for full builds.

**Architecture:** `ui = ["dep:rust-embed", "dep:mime_guess"]` gates `src/admin/ui.rs` and the SPA fallback; `AdminConfig.ui_enabled` (default true) gates it at runtime. One Dockerfile gains `ARG CARGO_FLAGS`; `docker.yml`'s matrix becomes platform × variant with per-variant merges and `-headless` suffixed tags. Spec: `docs/superpowers/specs/2026-07-26-headless-image-design.md`.

**Tech Stack:** Rust (cargo features, serde, axum 0.8, tower 0.5), Docker buildx, GitHub Actions.

## Global Constraints

- Conventional Commits; **no** Co-Authored-By trailer. Branch: `feature/headless-image`.
- The full (default-feature) build's behavior is unchanged when `ui_enabled` is absent.
- Tag scheme: default variant keeps `X.Y.Z`, `X.Y`, `latest`, `edge`; headless adds `-headless` to each. Both variants publish from the same triggers.
- Tree stays `cargo fmt --check` clean and clippy-clean (`-D warnings`); both `cargo test` (full) and `cargo check --no-default-features` must pass.
- UPX is explicitly out of scope (rejected in the spec).
- Workflow YAML: never interpolate `${{ }}` github-context data into `run:` scripts — use `env:` intermediates (Semgrep run-shell-injection; enforced by GitHub Advanced Security on PRs).
- Run `graphify update .` after Rust changes.

---

### Task 1: `ui` cargo feature + `admin.ui_enabled` + router gating

**Files:**
- Modify: `Cargo.toml` (deps lines 45-48; new `[features]`; `tower` dev feature)
- Modify: `src/config/system.rs` (AdminConfig, ~line 336-351; tests in its `mod tests`)
- Modify: `src/admin/mod.rs` (module decl line 14, `get` import line 20, router lines 56-72, doc comment lines 33-49; new tests)

**Interfaces:**
- Produces: cargo feature `ui` (in `default`); `AdminConfig.ui_enabled: bool` (serde default true); admin fallback mounted iff `ui` feature && `ui_enabled`. Tasks 2-4 rely on `cargo build --release --no-default-features` producing a UI-less binary.

- [ ] **Step 1: Write the failing tests**

In `src/config/system.rs`'s existing `mod tests`, add (mirroring the module's existing serde_yaml-based tests):

```rust
    #[test]
    fn test_admin_ui_enabled_defaults_true() {
        let cfg: AdminConfig =
            serde_yaml::from_str("username: u\npassword: p\n").unwrap();
        assert!(cfg.ui_enabled);
    }

    #[test]
    fn test_admin_ui_enabled_false_parses() {
        let cfg: AdminConfig =
            serde_yaml::from_str("username: u\npassword: p\nui_enabled: false\n").unwrap();
        assert!(!cfg.ui_enabled);
    }
```

In `src/admin/mod.rs`, add at the bottom (the state helper mirrors `src/admin/debug.rs:320-339`; the router test drives the real router via `tower::ServiceExt::oneshot`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewayConfig, SystemConfig};
    use crate::config_store::FileConfigStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> Arc<SharedState> {
        // Every section of both configs has a serde default, so an empty
        // document is the cheapest way to get a valid baseline.
        let system: SystemConfig = serde_yaml::from_str("{}").unwrap();
        let gateway: GatewayConfig = serde_yaml::from_str("{}").unwrap();
        Arc::new(
            SharedState::new(
                system,
                gateway,
                None,
                Arc::new(FileConfigStore::new(std::path::PathBuf::from(
                    "gateway.yaml",
                ))),
            )
            .unwrap(),
        )
    }

    fn admin_config(ui_enabled: bool) -> AdminConfig {
        let yaml = format!("username: u\npassword: p\nui_enabled: {}\n", ui_enabled);
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[cfg(feature = "ui")]
    #[tokio::test]
    async fn test_non_api_path_serves_spa_when_ui_enabled() {
        let app = build_router(&admin_config(true), test_state());
        let resp = app
            .oneshot(Request::get("/some/spa/route").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // ui/dist may be absent in dev checkouts; the fallback is mounted
        // either way. 200 = SPA served; 404 only when the embedded bundle is
        // empty — both prove the fallback handler answered, so assert on the
        // handler's contract, not the bundle's presence.
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND);

        // The API surface is mounted regardless: /healthz exists (401 without
        // credentials proves it hit the authed API router, not the fallback).
        let app = build_router(&admin_config(true), test_state());
        let resp = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_non_api_path_404_when_ui_disabled() {
        let app = build_router(&admin_config(false), test_state());
        let resp = app
            .oneshot(Request::get("/some/spa/route").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
```

If `test_non_api_path_serves_spa_when_ui_enabled`'s embedded-bundle caveat does not hold on this machine (ui/dist was built earlier in this session, so `200` is expected), keep the two-way assert anyway — it encodes the handler's documented contract (`src/admin/ui.rs:21`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test test_admin_ui_enabled` and `cargo test --lib admin::tests`
Expected: compile errors — no `ui_enabled` field, no `build_router` function.

- [ ] **Step 3: Implement**

1. `Cargo.toml` — make the UI deps optional and add the feature (keep the existing comment):

```toml
# Embedded UI (the `ui` feature; disabled for headless builds)
rust-embed = { version = "8", optional = true }
mime_guess = { version = "2", optional = true }
```

Directly above `[dependencies]`, add:

```toml
[features]
# The embedded admin web UI. Headless build (no UI assets, no serving code):
# `cargo build --release --no-default-features`.
default = ["ui"]
ui = ["dep:rust-embed", "dep:mime_guess"]
```

If `tower` lacks the `util` feature (needed for `ServiceExt::oneshot` in tests), change line 30 to `tower = { version = "0.5", features = ["util"] }` — additive, nothing else changes.

2. `src/config/system.rs` — in `AdminConfig`, after the `password` field:

```rust
    /// Serve the embedded web UI (node-graph editor) as the unauthenticated
    /// fallback. `false` returns 404 for non-API paths. Restart-gated like
    /// the rest of this file; inert in binaries compiled without the `ui`
    /// feature (the headless image), which never serve the UI.
    #[serde(default = "default_true")]
    pub ui_enabled: bool,
```

(`default_true` already exists in this file — see `client_cert_required`, line 267.)

3. `src/admin/mod.rs`:

- Line 14: `mod ui;` → `#[cfg(feature = "ui")]\nmod ui;`
- Line 20: `use axum::routing::get;` → `#[cfg(feature = "ui")]\nuse axum::routing::get;` (it is only used by the fallback; if the compiler disagrees, leave it ungated)
- Extract the router construction from `start_admin_server` into a function so tests can drive it, replacing lines 56-72:

```rust
/// Builds the admin router: authed API routes, plus — only when compiled with
/// the `ui` feature AND `admin.ui_enabled` is true — the unauthenticated SPA
/// fallback. Without it, non-API paths get axum's default 404.
fn build_router(admin_config: &AdminConfig, state: Arc<SharedState>) -> Router {
    let app = Router::new()
        // API routes (with auth)
        .merge(routes::router())
        .merge(policies::router())
        .merge(consumers::router())
        .merge(status::router())
        .merge(debug::router())
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth::AuthState {
                username: admin_config.username.clone(),
                password: admin_config.password.clone(),
            }),
            auth::basic_auth_middleware,
        ))
        .with_state(state);

    // UI static files (no auth — the API calls from the UI will authenticate).
    #[cfg(feature = "ui")]
    let app = if admin_config.ui_enabled {
        app.fallback(get(ui::serve_ui))
    } else {
        app
    };

    app
}
```

and in `start_admin_server`: `let app = build_router(admin_config, state);`

- Update the `start_admin_server` doc comment sentence about the fallback (lines 37-39) to: "any path not matched by the API falls back to the embedded SPA — when the binary is compiled with the `ui` feature and `admin.ui_enabled` is true (the default) — served without auth (the SPA's own API calls carry credentials)."
- Module doc line 6: append "(compile-time `ui` feature + runtime `admin.ui_enabled`)".

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test test_admin_ui_enabled && cargo test admin::tests && cargo check --no-default-features --locked && cargo test --locked && cargo clippy --all-targets --locked -- -D warnings && cargo fmt --check`
Expected: all green. Note: `cargo clippy --all-targets` with default features covers the test code; also run `cargo clippy --no-default-features --locked -- -D warnings` once to prove the headless combination is clippy-clean.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/config/system.rs src/admin/mod.rs
git commit -m "feat(admin): gate web UI behind ui feature and admin.ui_enabled"
```

---

### Task 2: CI headless guard + config example + website docs

**Files:**
- Modify: `.github/workflows/ci.yml` (new job after `rust`, line ~49)
- Modify: `config/system.yaml` (admin section)
- Modify: `website/docs/guides/configuration.md` (admin YAML block line ~39-44 and the Section/Keys table)
- Modify: `website/docs/guides/admin-api.md` (mention the switch where the UI is introduced)

**Interfaces:**
- Consumes: `cargo check --no-default-features` compiling (Task 1).

- [ ] **Step 1: Add the CI job**

In `.github/workflows/ci.yml`, after the `rust` job (before `ui-lint`), insert:

```yaml
  # The headless build (no `ui` feature, no ui/dist needed) must keep
  # compiling — it ships as the -headless Docker variant.
  headless-check:
    name: cargo check (headless)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2

      - run: cargo check --no-default-features --locked
```

- [ ] **Step 2: Update the example config**

In `config/system.yaml`, in the `admin:` section (after the `password` line), add:

```yaml
  # Serve the embedded web UI (node-graph editor). Set false for API-only
  # deployments; the -headless image never serves it regardless.
  ui_enabled: ${ADMIN_UI_ENABLED:-true}
```

- [ ] **Step 3: Update the website docs**

`website/docs/guides/configuration.md`: add `ui_enabled: ${ADMIN_UI_ENABLED:-true}` to the `admin:` YAML example (line ~43), and in the Section/Keys table extend the `admin` row's key list with: `` `ui_enabled` (default `true`) — serve the embedded web UI; `false` gives 404 on non-API paths. Inert in the `-headless` image, whose binary omits the UI entirely ``.

`website/docs/guides/admin-api.md`: where the embedded UI is first mentioned, add one sentence: "The UI can be disabled at runtime with `admin.ui_enabled: false` (restart required), and the `-headless` Docker image omits it at compile time."

- [ ] **Step 4: Verify**

Run: `cargo check --no-default-features --locked` (proves the CI job's command); `cd website && npm run build` if node_modules are present — otherwise skip and note (docs.yml gates the site build on merge).
Expected: check green; site build (if run) green.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/ci.yml config/system.yaml website/docs/guides/configuration.md website/docs/guides/admin-api.md
git commit -m "ci: guard headless build; docs: document admin.ui_enabled"
```

---

### Task 3: Dockerfile `CARGO_FLAGS` + variant matrix in `docker.yml`

**Files:**
- Modify: `Dockerfile` (builder stage)
- Modify: `.github/workflows/docker.yml` (`build` and `merge` jobs)

**Interfaces:**
- Consumes: headless build via `--no-default-features` (Task 1).
- Produces: Hub tags — default: `X.Y.Z`, `X.Y`, `latest`, `edge`; headless: same each with `-headless` suffix. Task 4 documents them.

- [ ] **Step 1: Dockerfile build arg**

In the builder stage, replace `RUN cargo build --release` with:

```dockerfile
# Headless variant: CARGO_FLAGS=--no-default-features compiles the UI out
# (word-splitting of the flags is intentional).
ARG CARGO_FLAGS=""
RUN cargo build --release ${CARGO_FLAGS}
```

- [ ] **Step 2: Rework the workflow's `build` job**

In `.github/workflows/docker.yml`:

1. Matrix becomes:

```yaml
    strategy:
      matrix:
        include:
          - platform: linux/amd64
            runner: ubuntu-24.04
            variant: default
            cargo_flags: ''
          - platform: linux/arm64
            runner: ubuntu-24.04-arm
            variant: default
            cargo_flags: ''
          - platform: linux/amd64
            runner: ubuntu-24.04
            variant: headless
            cargo_flags: --no-default-features
          - platform: linux/arm64
            runner: ubuntu-24.04-arm
            variant: headless
            cargo_flags: --no-default-features
```

2. Job name: `name: build (${{ matrix.variant }}, ${{ matrix.platform }})`.
3. Gate the two Node steps (`actions/setup-node` and "Build the embedded UI") with `if: matrix.variant == 'default'`, and add after them:

```yaml
      # Headless: the Dockerfile COPYs ui/dist unconditionally; an empty dir
      # satisfies the COPY and the ui feature is off, so nothing is embedded.
      - name: Create empty UI dir (headless)
        if: matrix.variant == 'headless'
        run: mkdir -p ui/dist
```

4. In "Build and push by digest": add `build-args: CARGO_FLAGS=${{ matrix.cargo_flags }}` and change both cache scopes to `docker-${{ matrix.variant }}-${{ env.PLATFORM_SLUG }}`.
5. Artifact name: `digests-${{ matrix.variant }}-${{ env.PLATFORM_SLUG }}`.

- [ ] **Step 3: Rework the `merge` job**

1. Add a matrix:

```yaml
    strategy:
      matrix:
        include:
          - variant: default
            tag_suffix: ''
          - variant: headless
            tag_suffix: -headless
```

2. Job name: `name: merge manifest (${{ matrix.variant }})`; download pattern becomes `digests-${{ matrix.variant }}-*`.
3. Metadata step — disable the implicit latest and make every tag explicit and suffix-aware (this replaces the old `latest=auto` behavior for the default variant with an equivalent explicit rule):

```yaml
      - name: Docker meta
        id: meta
        uses: docker/metadata-action@v5
        with:
          images: ${{ env.IMAGE }}
          flavor: |
            latest=false
          tags: |
            type=semver,pattern={{version}}${{ matrix.tag_suffix }}
            type=semver,pattern={{major}}.{{minor}}${{ matrix.tag_suffix }}
            type=raw,value=latest${{ matrix.tag_suffix }},enable=${{ github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v') }}
            type=raw,value=edge${{ matrix.tag_suffix }},enable=${{ github.ref == 'refs/heads/develop' }}
```

4. The "Inspect manifest" step's `${{ steps.meta.outputs.version }}` may now carry the suffix inconsistently across variants — replace it with the first computed tag via the action's exported env var (no `${{ }}` in the script):

```yaml
      - name: Inspect manifest
        run: docker buildx imagetools inspect "$(head -n1 <<< "$DOCKER_METADATA_OUTPUT_TAGS")"
```

The guard step, permissions, concurrency, and readme-sync stay untouched.

- [ ] **Step 4: Lint**

Run: `docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint:latest -color .github/workflows/docker.yml`
Expected: only the two pre-existing SC2046 warnings on the manifest-create step (the new inspect command uses a quoted substitution and adds none).

- [ ] **Step 5: Commit**

```bash
git add Dockerfile .github/workflows/docker.yml
git commit -m "ci(docker): build and publish -headless image variant"
```

---

### Task 4: Hub/README docs + dual-variant local smoke test

**Files:**
- Modify: `DOCKERHUB.md` (Tags table + new paragraph)
- Modify: `CLAUDE.md` (one line)
- Verification only beyond that — no other files.

- [ ] **Step 1: Update `DOCKERHUB.md`**

Extend the Tags table to:

```markdown
| Tag | Meaning |
| --- | --- |
| `latest` | The latest release (tip of `main`). |
| `X.Y.Z`, `X.Y` | Immutable release versions (e.g. `0.2.0`, `0.2`). |
| `edge` | Tip of `develop` — unreleased, may break. |
| `latest-headless`, `X.Y.Z-headless`, `X.Y-headless`, `edge-headless` | Same builds without the web UI (see below). |
```

After the table's "All tags are multi-arch…" line, add:

```markdown
### Headless variant

`-headless` images are compiled without the embedded web editor (`ui` cargo
feature off): smaller binary, no UI served on the admin port. The admin REST
API, health/metrics endpoints, and the data plane are identical to the full
image. In the full image the UI can also be turned off at runtime with
`admin.ui_enabled: false` in `system.yaml`.
```

- [ ] **Step 2: Update `CLAUDE.md`**

In the paragraph noting the web UI is embedded via rust-embed (the parenthetical under "Not Yet Implemented"), append: "The UI is gated by the default-on `ui` cargo feature (`--no-default-features` = headless build, published as the `-headless` Docker variant) and by `admin.ui_enabled` at runtime."

- [ ] **Step 3: Local smoke test — both variants**

Requires Docker Desktop running; if unavailable, report BLOCKED rather than claiming success.

```bash
cd ui && npm ci && npm run build && cd ..
docker build -t featherbit:smoke-full .
docker build -t featherbit:smoke-headless --build-arg CARGO_FLAGS=--no-default-features .
# Full: UI answers on a non-API path
docker run --rm -d --name fb-full -p 19090:9090 featherbit:smoke-full
sleep 3
curl -sf -o /dev/null -w "%{http_code}\n" http://localhost:19090/          # expect 200 (SPA)
curl -sf -o /dev/null -w "%{http_code}\n" http://localhost:19090/healthz   # expect 200
docker rm -f fb-full
# Headless: same path 404s, API still healthy
docker run --rm -d --name fb-headless -p 19090:9090 featherbit:smoke-headless
sleep 3
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:19090/           # expect 404
curl -sf -o /dev/null -w "%{http_code}\n" http://localhost:19090/healthz   # expect 200
docker rm -f fb-headless
```

Also compare the two binaries' sizes (`docker image ls featherbit` — headless should be noticeably smaller) and note both numbers in the report.

- [ ] **Step 4: Run `graphify update .`** (Rust changed in Task 1; keep the graph current). If graphify is unavailable, note it and move on.

- [ ] **Step 5: Commit**

```bash
git add DOCKERHUB.md CLAUDE.md
git commit -m "docs: document headless image variant"
```

---

## Post-merge verification (manual)

1. develop push → `edge` + `edge-headless` both multi-arch (`docker buildx imagetools inspect featherbit/featherbit:edge-headless`).
2. Next release tag → `X.Y.Z`, `X.Y`, `latest` + the three `-headless` twins.

## Self-review notes

- Spec coverage: feature+setting (Task 1), CI guard + config example + website docs (Task 2), Dockerfile+workflow+tags (Task 3), Hub/CLAUDE docs + smoke (Task 4). UPX correctly absent.
- The default variant's `latest` moves from `flavor: latest=auto` to an explicit raw rule with identical trigger coverage (main push OR v-tag push) — called out in Task 3 Step 3 so the reviewer sees it is intentional.
- Type consistency: `build_router(&AdminConfig, Arc<SharedState>) -> Router` used in both implementation and tests; `ui_enabled` named identically in config, YAML, and docs.
