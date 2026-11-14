param([Parameter(Mandatory)][string]$Tag)
$ErrorActionPreference = 'Stop'
# Catalog format 1 supports plain x.y.z only; reject prereleases and build metadata.
$match = [regex]::Match($Tag, '\A(?<addon>[a-z][a-z0-9]*(?:-[a-z0-9]+)*)-v(?<version>(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))\z')
if (!$match.Success) { throw 'Expected <addon>-v<major.minor.patch>' }
$addon = $match.Groups['addon'].Value
$version = $match.Groups['version'].Value
git fetch origin main --no-tags
if ($LASTEXITCODE -ne 0) { throw 'Cannot fetch main' }
git merge-base --is-ancestor HEAD origin/main
if ($LASTEXITCODE -ne 0) { throw 'The tagged commit must be on main' }
$manifestPath = "addons/$addon/Cargo.toml"
if (!(Test-Path -LiteralPath $manifestPath)) { throw 'Unknown add-on' }
$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'Cargo metadata failed' }
$expectedPath = (Resolve-Path -LiteralPath $manifestPath).Path.Replace('\', '/')
$package = @($metadata.packages | Where-Object { $_.manifest_path.Replace('\', '/') -eq $expectedPath })
if ($package.Count -ne 1 -or $package[0].name -ne $addon -or $package[0].version -ne $version) {
    throw 'Folder, crate name and tagged version must agree'
}
$bins = @($package[0].targets | Where-Object { $_.kind -contains 'bin' })
if ($bins.Count -ne 1 -or $bins[0].name -ne $addon) { throw 'One matching executable is required' }
$manifest = Get-Content -LiteralPath "addons/$addon/$addon.tbaddon" -Raw | ConvertFrom-Json
if ($manifest.manifest.version -ne $version) { throw 'Add-on manifest version differs from tag' }
"addon=$addon"
"version=$version"
