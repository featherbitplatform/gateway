# Docker Hub Publishing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish multi-arch (amd64+arm64) `featherbit/featherbit` images to Docker Hub from GitHub Actions, and keep the Docker Hub repo page documented.

**Architecture:** One new workflow (`docker.yml`): a matrix `build` job compiles each arch natively (no QEMU) and pushes by digest; a `merge` job stitches digests into one multi-arch manifest with the gitflow tag scheme; an independent `readme-sync` job pushes `DOCKERHUB.md` to the Hub description on `main` pushes. Spec: `docs/superpowers/specs/2026-07-26-docker-publish-design.md`.

**Tech Stack:** GitHub Actions (docker/setup-buildx-action@v3, docker/login-action@v3, docker/metadata-action@v5, docker/build-push-action@v6, peter-evans/dockerhub-description@v4), existing two-stage Dockerfile (rust:alpine → scratch).

## Global Constraints

- Conventional Commits; **no** Co-Authored-By trailer (CLAUDE.md).
- Work happens on the `feature/docker-publish` branch (gitflow).
- The workflow never hardcodes the Docker Hub namespace — always `${{ secrets.DOCKER_USERNAME }}`; documentation files use the literal `featherbit/featherbit`.
- Tag scheme (exact): tag `vX.Y.Z` → `X.Y.Z`, `X.Y`, `latest`; `main` push → `latest`; `develop` push → `edge`.
- The embedded UI must be built (`npm ci && npm run build` in `ui/`) before `docker build` — `ui/dist` is gitignored (documented trap in `ci.yml`/`security.yml`).
- No Rust code changes in this feature — do not run `cargo test`; verification is workflow lint + local `docker build`.

---

### Task 1: `.github/workflows/docker.yml`

**Files:**
- Create: `.github/workflows/docker.yml`

**Interfaces:**
- Consumes: repo secrets `DOCKER_USERNAME`, `DOCKER_PASSWORD`; the root `Dockerfile`; `ui/` npm project.
- Produces: Docker Hub images `${DOCKER_USERNAME}/featherbit` with tags per the Global Constraints scheme. Task 2's `readme-sync` job is part of this same file (added here, content file arrives in Task 2 — the job is gated to `main` pushes, so it cannot fire from this branch before `DOCKERHUB.md` exists on `main`; both land in the same merge).

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/docker.yml` with exactly this content:

```yaml
# Multi-arch Docker images to Docker Hub.
#
# Publishing model (gitflow):
#   vX.Y.Z tag   -> X.Y.Z, X.Y, latest
#   main push    -> latest
#   develop push -> edge
#   workflow_dispatch -> whatever the dispatched ref computes (backfills a tag)
#
# Each platform builds natively on its own runner (ubuntu-24.04 /
# ubuntu-24.04-arm; no QEMU) and pushes by digest only; `merge` stitches the
# digests into one multi-arch manifest and applies the tags. The Hub page
# (DOCKERHUB.md) syncs on main pushes, independent of image publishing.
name: docker

on:
  push:
    branches: [main, develop]
    tags: ['v*.*.*']
  workflow_dispatch:

concurrency:
  group: docker-${{ github.ref }}
  cancel-in-progress: true

env:
  IMAGE: docker.io/${{ secrets.DOCKER_USERNAME }}/featherbit

jobs:
  build:
    name: build (${{ matrix.platform }})
    strategy:
      matrix:
        include:
          - platform: linux/amd64
            runner: ubuntu-24.04
          - platform: linux/arm64
            runner: ubuntu-24.04-arm
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-node@v4
        with:
          node-version: 20
          cache: npm
          cache-dependency-path: ui/package-lock.json

      # The Dockerfile COPYs ui/dist into the builder; ui/dist is gitignored,
      # so it must be built first (same trap as in ci.yml / security.yml).
      - name: Build the embedded UI
        run: npm ci && npm run build
        working-directory: ui

      # "linux/amd64" -> "linux-amd64" for artifact names and cache scopes.
      - name: Prepare platform slug
        run: |
          platform='${{ matrix.platform }}'
          echo "PLATFORM_SLUG=${platform//\//-}" >> "$GITHUB_ENV"

      - uses: docker/setup-buildx-action@v3

      - uses: docker/login-action@v3
        with:
          username: ${{ secrets.DOCKER_USERNAME }}
          password: ${{ secrets.DOCKER_PASSWORD }}

      - name: Docker meta
        id: meta
        uses: docker/metadata-action@v5
        with:
          images: ${{ env.IMAGE }}

      - name: Build and push by digest
        id: build
        uses: docker/build-push-action@v6
        with:
          context: .
          platforms: ${{ matrix.platform }}
          labels: ${{ steps.meta.outputs.labels }}
          outputs: type=image,name=${{ env.IMAGE }},push-by-digest=true,name-canonical=true,push=true
          cache-from: type=gha,scope=docker-${{ env.PLATFORM_SLUG }}
          cache-to: type=gha,scope=docker-${{ env.PLATFORM_SLUG }},mode=max

      - name: Export digest
        run: |
          mkdir -p "${{ runner.temp }}/digests"
          digest='${{ steps.build.outputs.digest }}'
          touch "${{ runner.temp }}/digests/${digest#sha256:}"

      - uses: actions/upload-artifact@v4
        with:
          name: digests-${{ env.PLATFORM_SLUG }}
          path: ${{ runner.temp }}/digests/*
          if-no-files-found: error
          retention-days: 1

  merge:
    name: merge manifest
    runs-on: ubuntu-24.04
    needs: build
    steps:
      - uses: actions/download-artifact@v4
        with:
          path: ${{ runner.temp }}/digests
          pattern: digests-*
          merge-multiple: true

      - uses: docker/setup-buildx-action@v3

      - uses: docker/login-action@v3
        with:
          username: ${{ secrets.DOCKER_USERNAME }}
          password: ${{ secrets.DOCKER_PASSWORD }}

      - name: Docker meta
        id: meta
        uses: docker/metadata-action@v5
        with:
          images: ${{ env.IMAGE }}
          tags: |
            type=semver,pattern={{version}}
            type=semver,pattern={{major}}.{{minor}}
            type=raw,value=latest,enable={{is_default_branch}}
            type=raw,value=edge,enable=${{ github.ref == 'refs/heads/develop' }}

      - name: Create multi-arch manifest
        working-directory: ${{ runner.temp }}/digests
        run: |
          docker buildx imagetools create \
            $(jq -cr '.tags | map("-t " + .) | join(" ")' <<< "$DOCKER_METADATA_OUTPUT_JSON") \
            $(printf '${{ env.IMAGE }}@sha256:%s ' *)

      - name: Inspect manifest
        run: docker buildx imagetools inspect '${{ env.IMAGE }}:${{ steps.meta.outputs.version }}'

  readme-sync:
    name: sync Docker Hub description
    if: github.ref == 'refs/heads/main' && github.event_name == 'push'
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4

      # Needs DOCKER_PASSWORD to be an account password or adequately scoped
      # PAT; with a restricted access token this job fails while images still
      # publish (no `needs` on purpose). Fix = rotate the secret.
      - uses: peter-evans/dockerhub-description@v4
        with:
          username: ${{ secrets.DOCKER_USERNAME }}
          password: ${{ secrets.DOCKER_PASSWORD }}
          repository: ${{ secrets.DOCKER_USERNAME }}/featherbit
          readme-filepath: ./DOCKERHUB.md
          short-description: Lightweight single-binary API gateway with node-graph routing policies
```

- [ ] **Step 2: Lint the workflow**

Run (Docker Desktop may not be running on this machine; if the `docker run` fails with a daemon error, note it and fall back to a careful manual re-read of the YAML against Step 1):

```bash
docker run --rm -v "$PWD:/repo" -w /repo rhysd/actionlint:latest -color .github/workflows/docker.yml
```

Expected: no findings (exit 0).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/docker.yml
git commit -m "ci: publish multi-arch Docker images to Docker Hub"
```

---

### Task 2: `DOCKERHUB.md` + README pointer

**Files:**
- Create: `DOCKERHUB.md`
- Modify: `README.md` (the `## Documentation` section, after the existing docs paragraph)

**Interfaces:**
- Consumes: Task 1's `readme-sync` job reads `./DOCKERHUB.md`.
- Produces: the Docker Hub repository description content.

- [ ] **Step 1: Create `DOCKERHUB.md`**

Exactly this content:

````markdown
# featherbit

A lightweight, high-performance API gateway delivered as a **single Rust
binary**. Routes are configured as visual node graphs — each plugin is a node,
wired together through success and error ports. The data-plane server, the
admin REST API, and the node-graph web editor are all served by the same
executable; this image ships it on a `scratch` base.

- **Source & issues:** https://github.com/featherbitplatform/gateway
- **Documentation:** https://featherbitplatform.github.io/gateway/
- **Plugin reference (80+ node types):** https://featherbitplatform.github.io/gateway/docs/reference/plugins

## Tags

| Tag | Meaning |
| --- | --- |
| `latest` | The latest release (tip of `main`). |
| `X.Y.Z`, `X.Y` | Immutable release versions (e.g. `0.2.0`, `0.2`). |
| `edge` | Tip of `develop` — unreleased, may break. |

All tags are multi-arch manifests for `linux/amd64` and `linux/arm64`.

## Quick start

```console
docker run --rm -p 8080:8080 -p 9090:9090 featherbit/featherbit
```

The image ships a working example configuration. The data plane listens on
**8080**, the admin API + web editor on **9090** (default credentials are in
the example `system.yaml` — change them before exposing anything).

With your own configuration:

```yaml
# compose.yaml
services:
  gateway:
    image: featherbit/featherbit:latest
    ports:
      - "8080:8080"
      - "9090:9090"
    volumes:
      - ./system.yaml:/etc/gateway/system.yaml:ro
      - ./gateway.yaml:/etc/gateway/gateway.yaml:ro
```

## Configuration

- Config lives at `/etc/gateway/system.yaml` (listeners, TLS, timeouts, admin
  API) and `/etc/gateway/gateway.yaml` (routes + node-graph policies); mount
  your own files over them.
- Every YAML value supports `${ENV_VAR:-default}` interpolation, so secrets
  and per-environment settings can come from the container environment.
- `gateway.yaml` hot-reloads on change; `system.yaml` changes need a restart.
- The container runs as non-root (uid 65532) from `scratch` — there is no
  shell inside the image.

Full configuration reference: https://featherbitplatform.github.io/gateway/
````

- [ ] **Step 2: Add the README pointer**

In `README.md`, directly after the `## Documentation` paragraph (the one linking featherbitplatform.github.io), add this new paragraph:

```markdown
Container images are on [Docker Hub](https://hub.docker.com/r/featherbit/featherbit): `docker pull featherbit/featherbit` (`latest` = newest release, `edge` = tip of develop, `X.Y.Z` = pinned releases; amd64 + arm64). Published by `.github/workflows/docker.yml`, with the Hub page synced from [`DOCKERHUB.md`](DOCKERHUB.md).
```

- [ ] **Step 3: Verify rendering**

Run: `npx --yes markdownlint-cli2 DOCKERHUB.md || true` — informational only (the repo has no markdownlint config; just confirm no syntax-level surprises in the output). Then visually re-read both files.

- [ ] **Step 4: Commit**

```bash
git add DOCKERHUB.md README.md
git commit -m "docs: add Docker Hub overview page and README pointer"
```

---

### Task 3: local Docker build smoke test

**Files:** none changed — verification only.

**Interfaces:**
- Consumes: the root `Dockerfile`, `ui/` npm project.

- [ ] **Step 1: Build the embedded UI**

```bash
cd ui && npm ci && npm run build && cd ..
```

Expected: `ui/dist/` exists and is non-empty.

- [ ] **Step 2: Build the image locally**

Requires a running Docker daemon (Docker Desktop on this machine). If the daemon is unavailable, report that the smoke test could not run — do NOT mark this task complete silently.

```bash
docker build -t featherbit:smoke .
```

Expected: builds to completion (the Rust release compile takes several minutes).

- [ ] **Step 3: Smoke-run**

```bash
docker run --rm -d --name fb-smoke -p 18080:8080 -p 19090:9090 featherbit:smoke
sleep 3
curl -sf http://localhost:19090/healthz && echo HEALTHY
docker rm -f fb-smoke
```

Expected: `HEALTHY` printed. If `/healthz` needs auth or a different port per `config/system.yaml`, check the container logs (`docker logs fb-smoke`) and verify liveness from those instead — report what you saw.

- [ ] **Step 4: No commit** — nothing changed; note results in the report.

---

## Post-merge verification (manual, after this branch merges to develop / release reaches main)

1. Push to `develop` fires the workflow → `featherbit/featherbit:edge`; check with `docker buildx imagetools inspect featherbit/featherbit:edge` (expect amd64 + arm64 entries).
2. Backfill the existing release: `gh workflow run docker.yml --ref v0.2.0` → `0.2.0`, `0.2`, `latest`.
3. On the next release merge to `main`: `readme-sync` publishes the Hub page; if it fails with an auth error, `DOCKER_PASSWORD` is a scoped token — rotate to a password/PAT.

## Self-review notes

- Spec coverage: workflow triggers/tag scheme/platforms/digest-merge (Task 1), DOCKERHUB.md sections + README pointer (Task 2), local verification (Task 3), post-merge steps documented above. Out-of-scope items (ghcr, signing) untouched.
- `readme-sync` lives in Task 1's file but its content file arrives in Task 2 — safe because the job only fires on `main` pushes, and both tasks merge together (noted in Task 1 Interfaces).
- No `cargo test` on purpose: no Rust changes; the suite adds nothing here.
