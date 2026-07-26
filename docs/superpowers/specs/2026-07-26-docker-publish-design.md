# Design: Docker Hub publishing + Hub overview page

**Date:** 2026-07-26
**Branch:** `feature/docker-publish`
**Status:** approved

## Problem

Releases are tagged on `main` (gitflow, e.g. `v0.2.0`) but no container images are
published. Users must clone and build. Docker Hub secrets already exist in the
GitHub repo: `DOCKER_USERNAME` and `DOCKER_PASSWORD`. The Docker Hub repository
page also needs real documentation.

## Decisions

| Decision | Choice |
|---|---|
| Image name | `${DOCKER_USERNAME}/featherbit` (namespace never hardcoded in the workflow). Confirmed namespace: `featherbit`, so the public image is `featherbit/featherbit`; documentation uses the literal. |
| Publish triggers | `v*.*.*` tags; every push to `main`; every push to `develop`; `workflow_dispatch` for backfill |
| Docker tags | tag `vX.Y.Z` → `X.Y.Z`, `X.Y`, `latest`; `main` push → `latest`; `develop` push → `edge` |
| Platforms | `linux/amd64` + `linux/arm64`, multi-arch manifest |
| Build strategy | Native per-arch runners (`ubuntu-24.04`, `ubuntu-24.04-arm`) pushing by digest, then a manifest-merge job (approach A). No QEMU. |
| Hub page | `DOCKERHUB.md` at repo root, synced by `peter-evans/dockerhub-description@v4` on `main` pushes |

## Workflow: `.github/workflows/docker.yml`

```yaml
on:
  push:
    branches: [main, develop]
    tags: ['v*.*.*']
  workflow_dispatch:
concurrency:
  group: docker-${{ github.ref }}
  cancel-in-progress: true
```

### Job `build` (matrix)

Matrix: `{platform: linux/amd64, runner: ubuntu-24.04}`,
`{platform: linux/arm64, runner: ubuntu-24.04-arm}` (GitHub's free arm64 runners
for public repos — native builds, no emulation).

Steps per leg:
1. `actions/checkout`.
2. Node 20 with npm cache (`cache-dependency-path: ui/package-lock.json`), then
   `npm ci && npm run build` in `ui/`. **Required**: the Dockerfile `COPY`s the
   gitignored `ui/dist/` into the builder — same trap already documented in
   `ci.yml` and `security.yml`.
3. `docker/setup-buildx-action`.
4. `docker/login-action` with `DOCKER_USERNAME`/`DOCKER_PASSWORD`.
5. `docker/metadata-action` (labels/annotations only in this job).
6. `docker/build-push-action`: `platforms: ${{ matrix.platform }}`,
   `outputs: type=image,name=docker.io/${{ secrets.DOCKER_USERNAME }}/featherbit,push-by-digest=true,name-canonical=true,push=true`,
   GHA cache: `cache-from: type=gha,scope=docker-${{ matrix.platform }}`,
   `cache-to: type=gha,scope=docker-${{ matrix.platform }},mode=max`.
7. Write the image digest to `digests/` and upload as artifact
   (`digests-linux-amd64` / `digests-linux-arm64`).

### Job `merge` (needs: build)

1. Download both digest artifacts.
2. `docker/login-action`.
3. `docker/metadata-action` with:
   - `images: docker.io/${{ secrets.DOCKER_USERNAME }}/featherbit`
   - `tags:` —
     `type=semver,pattern={{version}}` ·
     `type=semver,pattern={{major}}.{{minor}}` ·
     `type=raw,value=latest,enable={{is_default_branch}}` ·
     `type=raw,value=edge,enable=${{ github.ref == 'refs/heads/develop' }}`
   - flavor `latest=auto` (semver tag events also get `latest`; the raw rule
     covers plain `main` pushes).
4. `docker buildx imagetools create` with all computed `-t` tags and the two
   `@sha256:` digests.
5. `docker buildx imagetools inspect` on the first tag to verify the manifest
   lists both platforms.

### Job `readme-sync`

- `if: github.ref == 'refs/heads/main' && github.event_name == 'push'`
- Independent of `build`/`merge` (no `needs`) — a sync failure never blocks
  image publishing.
- `peter-evans/dockerhub-description@v4` with
  `repository: ${{ secrets.DOCKER_USERNAME }}/featherbit`,
  `readme-filepath: ./DOCKERHUB.md`, and
  `short-description: "Lightweight single-binary API gateway with node-graph routing policies"`.
- **Known caveat:** the action needs `DOCKER_PASSWORD` to be an account
  password or a PAT with adequate scope. If the secret is a restricted access
  token, this job fails visibly while images still publish; the fix is
  rotating the secret, not changing the workflow.

`workflow_dispatch` runs publish whatever ref they are dispatched on
(`gh workflow run docker.yml --ref v0.2.0` backfills the existing release).

## `DOCKERHUB.md` (repo root)

Sections, in order (total well under Docker Hub's 25k-char limit; no relative
images — Hub does not resolve them):

1. **featherbit** — one-paragraph overview (single Rust binary, node-graph
   routing policies, admin API + embedded web editor).
2. **Tags** — table: `latest` (latest release / main), `edge` (tip of develop,
   unstable), `X.Y.Z` / `X.Y` (immutable releases).
3. **Quick start** — `docker run -p 8080:8080 -p 9090:9090 featherbit/featherbit`
   plus a minimal `docker compose` example with mounted config.
4. **Configuration** — image ships baked-in example config at `/etc/gateway/`;
   mount your own `system.yaml`/`gateway.yaml` over it; all YAML values support
   `${ENV_VAR:-default}` interpolation; ports 8080 (data plane) / 9090 (admin
   API + UI); runs as non-root uid 65532 from `scratch`.
5. **Links** — GitHub repo, documentation site
   (https://featherbitplatform.github.io/gateway/), plugin reference.

## README touch-up

Add a "Docker" line to the README Documentation section: `docker pull
featherbit/featherbit` and a link to the Docker Hub page
(https://hub.docker.com/r/featherbit/featherbit).

## Error handling

- UI build failure or either arch build failure fails the run — nothing is
  tagged (digests without a manifest are unreferenced and harmless).
- `merge` failing after a partial `imagetools create` is re-runnable; tags are
  idempotent overwrites.
- `readme-sync` is isolated (see caveat above).

## Testing / verification

1. Lint the workflow (actionlint via `docker run rhysd/actionlint` if Docker is
   available locally; otherwise careful static review).
2. Local `docker build` smoke test (after `npm run build` in `ui/`) to confirm
   the Dockerfile still builds.
3. Real verification after merge to `develop`: the push publishes `edge`;
   inspect with `docker buildx imagetools inspect featherbit/featherbit:edge`
   (expect two platform entries).
4. Backfill: `gh workflow run docker.yml --ref v0.2.0`, then check `0.2.0`,
   `0.2`, `latest` on the Hub.
5. README sync verified on the release merge to `main`.

## Out of scope

- GitHub Container Registry (ghcr.io) mirroring.
- Image signing / provenance attestations (cosign, SLSA).
- Publishing the `dev/echo-backend` or `ui` dev images.
