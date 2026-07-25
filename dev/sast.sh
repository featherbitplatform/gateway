#!/usr/bin/env bash
# Local SAST pipeline — the same scanners and thresholds as
# .github/workflows/security.yml, run via Docker (plus native cargo-deny).
# Linux/macOS twin of dev/sast.ps1; keep the two in sync.
#
# Usage:
#   ./dev/sast.sh                 # all scans except the image scan
#   ./dev/sast.sh semgrep         # a single tool
#   ./dev/sast.sh image           # docker build + grype/trivy on the image
#
# Targets: semgrep deny grype hadolint gitleaks npm-audit image all
set -u

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# Tool images, pinned so local results match across machines. Bump deliberately.
SEMGREP_IMAGE='semgrep/semgrep:latest'
GRYPE_IMAGE='anchore/grype:latest'
HADOLINT_IMAGE='hadolint/hadolint:latest-alpine'
GITLEAKS_IMAGE='zricethezav/gitleaks:latest'
TRIVY_IMAGE='aquasec/trivy:latest'
NODE_IMAGE='node:22-alpine'

# Semgrep registry rulesets — keep in sync with the semgrep job in security.yml.
SEMGREP_RULESETS='--config p/default --config p/rust --config p/typescript --config p/dockerfile'
# A WebSocket proxy legitimately handles ws:// everywhere; this JS rule fires
# on the string even inside Rust comments.
SEMGREP_EXCLUDES='--exclude-rule javascript.lang.security.detect-insecure-websocket.detect-insecure-websocket'

TARGETS=("$@")
[ ${#TARGETS[@]} -eq 0 ] && TARGETS=(all)
if [ "${TARGETS[0]}" = all ]; then
    TARGETS=(semgrep deny grype hadolint gitleaks npm-audit)
fi

NAMES=()
CODES=()

run_scan() { # run_scan <name> <cmd...>
    local name="$1"; shift
    printf '\n=== %s ===\n' "$name"
    "$@"
    local code=$?
    NAMES+=("$name")
    CODES+=("$code")
}

for target in "${TARGETS[@]}"; do
    case "$target" in

    semgrep)
        # shellcheck disable=SC2086  # rulesets/excludes deliberately word-split
        run_scan semgrep docker run --rm -v "$REPO_ROOT:/src:ro" -w /src "$SEMGREP_IMAGE" \
            semgrep scan $SEMGREP_RULESETS $SEMGREP_EXCLUDES --severity ERROR --error --metrics=off
        ;;

    deny)
        if command -v cargo-deny >/dev/null 2>&1; then
            run_scan cargo-deny cargo deny --manifest-path "$REPO_ROOT/Cargo.toml" check
        else
            echo 'cargo-deny is not installed; run: cargo install cargo-deny --locked'
            NAMES+=(cargo-deny); CODES+=(1)
        fi
        ;;

    grype)
        # Named volume caches the vulnerability DB between runs.
        run_scan 'grype (filesystem)' docker run --rm -v "$REPO_ROOT:/src:ro" \
            -v featherbit-grype-db:/db -e GRYPE_DB_CACHE_DIR=/db \
            "$GRYPE_IMAGE" dir:/src -c /src/.grype.yaml
        ;;

    hadolint)
        run_scan hadolint docker run --rm -v "$REPO_ROOT:/src:ro" -w /src "$HADOLINT_IMAGE" \
            hadolint --config .hadolint.yaml \
            Dockerfile ui/Dockerfile dev/echo-backend/Dockerfile
        ;;

    gitleaks)
        # `dir` scans the working tree (not just git history), which also
        # covers files that are not committed yet.
        run_scan gitleaks docker run --rm -v "$REPO_ROOT:/src:ro" "$GITLEAKS_IMAGE" \
            dir /src --config /src/.gitleaks.toml --no-banner --redact
        ;;

    npm-audit)
        # audit-ci instead of raw `npm audit`: it honors the documented
        # allowlist in each tree's audit-ci.jsonc (npm audit cannot ignore).
        for dir in ui e2e website; do
            run_scan "npm audit ($dir)" docker run --rm -v "$REPO_ROOT/$dir:/app:ro" -w /app \
                "$NODE_IMAGE" npx --yes 'audit-ci@^7' --config audit-ci.jsonc
        done
        ;;

    image)
        if [ ! -f "$REPO_ROOT/ui/dist/index.html" ]; then
            echo 'ui/dist is missing (the Dockerfile embeds it). Build it first: cd ui && npm ci && npm run build'
            NAMES+=('image build'); CODES+=(1)
            continue
        fi
        printf '\n=== docker build ===\n'
        if ! docker build -t featherbit:sast "$REPO_ROOT"; then
            NAMES+=('image build'); CODES+=(1)
            continue
        fi
        NAMES+=('image build'); CODES+=(0)

        # Export once, scan the archive — avoids handing scanners the socket.
        mkdir -p "$REPO_ROOT/sast-out"
        docker save featherbit:sast -o "$REPO_ROOT/sast-out/featherbit-image.tar"

        run_scan 'grype (image)' docker run --rm -v "$REPO_ROOT:/src:ro" \
            -v featherbit-grype-db:/db -e GRYPE_DB_CACHE_DIR=/db \
            "$GRYPE_IMAGE" docker-archive:/src/sast-out/featherbit-image.tar -c /src/.grype.yaml
        run_scan 'trivy (image)' docker run --rm -v "$REPO_ROOT:/src:ro" \
            -v featherbit-trivy-cache:/root/.cache/trivy \
            "$TRIVY_IMAGE" image --config /src/trivy.yaml --exit-code 1 \
            --input /src/sast-out/featherbit-image.tar
        ;;

    *)
        echo "Unknown target '$target'. Targets: semgrep deny grype hadolint gitleaks npm-audit image all"
        exit 2
        ;;
    esac
done

printf '\n=== Summary ===\n'
failed=0
for i in "${!NAMES[@]}"; do
    if [ "${CODES[$i]}" -eq 0 ]; then
        printf '  PASS  %s\n' "${NAMES[$i]}"
    else
        printf '  FAIL  %s\n' "${NAMES[$i]}"
        failed=1
    fi
done
exit "$failed"
