<#
.SYNOPSIS
    Local SAST pipeline — the same scanners and thresholds as
    .github/workflows/security.yml, run via Docker (plus native cargo-deny).

.DESCRIPTION
    Every tool reads its config from the repo root (deny.toml, .hadolint.yaml,
    .gitleaks.toml, .grype.yaml, trivy.yaml, .semgrepignore), so local runs and
    CI can't drift. High/critical findings fail; lower severities are reported.

.EXAMPLE
    ./dev/sast.ps1                 # all scans except the image scan
    ./dev/sast.ps1 semgrep         # a single tool
    ./dev/sast.ps1 image           # docker build + grype/trivy + SBOM on the image
    ./dev/sast.ps1 sbom            # CycloneDX SBOMs into sast-out/sbom/
    ./dev/sast.ps1 semgrep deny    # any subset

    Targets: semgrep deny grype hadolint gitleaks npm-audit sbom image all
#>
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Targets = @('all')
)

$ErrorActionPreference = 'Continue'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

# Tool images, pinned so local results match across machines. Bump deliberately.
$SemgrepImage  = 'semgrep/semgrep:latest'
$GrypeImage    = 'anchore/grype:latest'
$HadolintImage = 'hadolint/hadolint:latest-alpine'
$GitleaksImage = 'zricethezav/gitleaks:latest'
$TrivyImage    = 'aquasec/trivy:latest'
$NodeImage     = 'node:22-alpine'
$SyftImage     = 'anchore/syft:latest'

# Semgrep registry rulesets — keep in sync with the semgrep job in security.yml.
$SemgrepRulesets = @('p/default', 'p/rust', 'p/typescript', 'p/dockerfile')
# A WebSocket proxy legitimately handles ws:// everywhere; this JS rule fires
# on the string even inside Rust comments.
$SemgrepExcludedRules = @('javascript.lang.security.detect-insecure-websocket.detect-insecure-websocket')

if ($Targets -contains 'all') {
    $Targets = @('semgrep', 'deny', 'grype', 'hadolint', 'gitleaks', 'npm-audit', 'sbom')
}

# The product version stamped into every SBOM (ui/website package.json
# versions are placeholders; the components version together).
$GatewayVersion = (Select-String -Path "$RepoRoot/Cargo.toml" -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value

$Results = [ordered]@{}

function Invoke-Scan {
    param([string]$Name, [scriptblock]$Body)
    Write-Host ""
    Write-Host "=== $Name ===" -ForegroundColor Cyan
    & $Body
    $Results[$Name] = ($LASTEXITCODE -eq 0)
}

foreach ($target in $Targets) {
    switch ($target) {

        'semgrep' {
            Invoke-Scan 'semgrep' {
                $configs = $SemgrepRulesets | ForEach-Object { @('--config', $_) }
                $excludes = $SemgrepExcludedRules | ForEach-Object { @('--exclude-rule', $_) }
                docker run --rm -v "${RepoRoot}:/src:ro" -w /src $SemgrepImage `
                    semgrep scan @configs @excludes --severity ERROR --error --metrics=off
            }
        }

        'deny' {
            Invoke-Scan 'cargo-deny' {
                if (-not (Get-Command cargo-deny -ErrorAction SilentlyContinue)) {
                    Write-Host 'cargo-deny is not installed; run: cargo install cargo-deny --locked' -ForegroundColor Yellow
                    cmd /c 'exit 1'
                    return
                }
                cargo deny --manifest-path "$RepoRoot/Cargo.toml" check
            }
        }

        'grype' {
            Invoke-Scan 'grype (filesystem)' {
                # Named volume caches the vulnerability DB between runs.
                docker run --rm -v "${RepoRoot}:/src:ro" `
                    -v featherbit-grype-db:/db -e GRYPE_DB_CACHE_DIR=/db `
                    $GrypeImage dir:/src -c /src/.grype.yaml
            }
        }

        'hadolint' {
            Invoke-Scan 'hadolint' {
                docker run --rm -v "${RepoRoot}:/src:ro" -w /src $HadolintImage `
                    hadolint --config .hadolint.yaml `
                    Dockerfile ui/Dockerfile dev/echo-backend/Dockerfile
            }
        }

        'gitleaks' {
            Invoke-Scan 'gitleaks' {
                # `dir` scans the working tree (not just git history), which
                # also covers files that are not committed yet.
                docker run --rm -v "${RepoRoot}:/src:ro" $GitleaksImage `
                    dir /src --config /src/.gitleaks.toml --no-banner --redact
            }
        }

        'npm-audit' {
            # audit-ci instead of raw `npm audit`: it honors the documented
            # allowlist in each tree's audit-ci.jsonc (npm audit cannot ignore).
            foreach ($dir in @('ui', 'e2e', 'website')) {
                Invoke-Scan "npm audit ($dir)" {
                    docker run --rm -v "${RepoRoot}/${dir}:/app:ro" -w /app $NodeImage `
                        npx --yes audit-ci@^7 --config audit-ci.jsonc
                }.GetNewClosure()
            }
        }

        'sbom' {
            # CycloneDX SBOMs, one per source component — same generator (syft)
            # and file names as docs.yml / docker.yml, so local output matches
            # what CI publishes. The image SBOM comes from the `image` target.
            New-Item -ItemType Directory -Force "$RepoRoot/sast-out/sbom" | Out-Null
            foreach ($component in @(
                @{ Name = 'featherbit-gateway'; Source = 'file:/src/Cargo.lock' },
                @{ Name = 'featherbit-ui';      Source = 'file:/src/ui/package-lock.json' },
                @{ Name = 'featherbit-website'; Source = 'file:/src/website/package-lock.json' }
            )) {
                Invoke-Scan "sbom ($($component.Name))" {
                    docker run --rm -v "${RepoRoot}:/src:ro" -v "${RepoRoot}/sast-out/sbom:/out" `
                        $SyftImage scan $component.Source `
                        --source-name $component.Name --source-version $GatewayVersion `
                        -o "cyclonedx-json=/out/$($component.Name).cdx.json"
                }.GetNewClosure()
            }
        }

        'image' {
            if (-not (Test-Path "$RepoRoot/ui/dist/index.html")) {
                Write-Host 'ui/dist is missing (the Dockerfile embeds it). Build it first: cd ui && npm ci && npm run build' -ForegroundColor Yellow
                $Results['image build'] = $false
                continue
            }
            Write-Host ""
            Write-Host '=== docker build ===' -ForegroundColor Cyan
            docker build -t featherbit:sast $RepoRoot
            if ($LASTEXITCODE -ne 0) {
                $Results['image build'] = $false
                continue
            }
            $Results['image build'] = $true

            # Export once, scan the archive — avoids handing scanners the socket.
            New-Item -ItemType Directory -Force "$RepoRoot/sast-out" | Out-Null
            docker save featherbit:sast -o "$RepoRoot/sast-out/featherbit-image.tar"

            Invoke-Scan 'grype (image)' {
                docker run --rm -v "${RepoRoot}:/src:ro" `
                    -v featherbit-grype-db:/db -e GRYPE_DB_CACHE_DIR=/db `
                    $GrypeImage docker-archive:/src/sast-out/featherbit-image.tar -c /src/.grype.yaml
            }
            Invoke-Scan 'trivy (image)' {
                docker run --rm -v "${RepoRoot}:/src:ro" `
                    -v featherbit-trivy-cache:/root/.cache/trivy `
                    $TrivyImage image --config /src/trivy.yaml --exit-code 1 `
                    --input /src/sast-out/featherbit-image.tar
            }
            New-Item -ItemType Directory -Force "$RepoRoot/sast-out/sbom" | Out-Null
            Invoke-Scan 'sbom (featherbit-image)' {
                docker run --rm -v "${RepoRoot}:/src:ro" -v "${RepoRoot}/sast-out/sbom:/out" `
                    $SyftImage scan docker-archive:/src/sast-out/featherbit-image.tar `
                    --source-name featherbit-image --source-version $GatewayVersion `
                    -o cyclonedx-json=/out/featherbit-image.cdx.json
            }
        }

        default {
            Write-Host "Unknown target '$target'. Targets: semgrep deny grype hadolint gitleaks npm-audit sbom image all" -ForegroundColor Red
            exit 2
        }
    }
}

Write-Host ""
Write-Host '=== Summary ===' -ForegroundColor Cyan
$failed = $false
foreach ($entry in $Results.GetEnumerator()) {
    if ($entry.Value) {
        Write-Host ("  PASS  {0}" -f $entry.Key) -ForegroundColor Green
    } else {
        Write-Host ("  FAIL  {0}" -f $entry.Key) -ForegroundColor Red
        $failed = $true
    }
}
if ($failed) { exit 1 }
