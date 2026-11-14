$ErrorActionPreference = 'Stop'
$check = Join-Path $PSScriptRoot 'check-release.ps1'
$fixture = Join-Path ([IO.Path]::GetTempPath()) ("toolboks-release-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $fixture | Out-Null
Push-Location $fixture
try {
    git init --initial-branch=main --quiet
    git config user.name 'Release test'
    git config user.email 'test@example.invalid'
    git remote add origin .
    New-Item -ItemType Directory -Force addons/hello/src | Out-Null
    @'
[workspace]
members = ["addons/hello"]
resolver = "2"
'@ | Set-Content Cargo.toml
    @'
[package]
name = "hello"
version = "0.1.0"
edition = "2021"
'@ | Set-Content addons/hello/Cargo.toml
    'fn main() {}' | Set-Content addons/hello/src/main.rs
    '{"manifest":{"version":"0.1.0"}}' | Set-Content addons/hello/hello.tbaddon
    cargo generate-lockfile
    if ($LASTEXITCODE -ne 0) { throw 'Fixture Cargo setup failed' }
    git add .
    git commit --quiet -m initial
    $actual = & $check -Tag 'hello-v0.1.0'
    if ($actual -notcontains 'addon=hello' -or $actual -notcontains 'version=0.1.0') { throw 'Valid tag failed' }
    foreach ($tag in @('hello-v0.1.0$(whoami)', 'hello-v01.1.0', 'hello-v0.1.0-preview', 'missing-v0.1.0', 'hello-v0.2.0')) {
        $rejected = $false
        try { & $check -Tag $tag | Out-Null } catch { $rejected = $true }
        if (!$rejected) { throw "Unsafe/invalid tag accepted: $tag" }
    }
    git switch --quiet -c unpublished
    'off main' | Set-Content unpublished.txt
    git add .
    git commit --quiet -m unpublished
    $rejected = $false
    try { & $check -Tag 'hello-v0.1.0' | Out-Null } catch { $rejected = $true }
    if (!$rejected) { throw 'Off-main commit accepted' }
    Write-Output 'Release validation: valid tag accepted; injection, malformed versions, unknown add-on, version mismatch and off-main commit rejected.'
} finally {
    Pop-Location
    $resolved = [IO.Path]::GetFullPath($fixture)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
    if (!$resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or !(Split-Path $resolved -Leaf).StartsWith('toolboks-release-test-')) {
        throw 'Refusing cleanup outside the test fixture'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}

# The expected off-main rejection leaves git's native exit code at 1. GitHub's
# PowerShell wrapper exits with that value even though we caught the exception.
# Clear it only after every assertion and cleanup have completed successfully.
$global:LASTEXITCODE = 0
