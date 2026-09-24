# Builds the Windows installer: target\installer\CrestronLoadRunner-<version>.msi
#
# Needs WiX 5: dotnet tool install --global wix
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

# The crate version is the MSI version, so it has to be one Windows Installer
# accepts: three numbers, the first at most 255, the second at most 255, and
# the third at most 65535.
$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
$parts = $version.Split('.')
if ($parts.Count -ne 3 -or [int]$parts[0] -gt 255 -or [int]$parts[1] -gt 255 -or [int]$parts[2] -gt 65535) {
    throw "Cargo.toml version $version cannot be an MSI version (YY.M.P, first two numbers at most 255)"
}

$exe = Join-Path $root 'target\release\crestron-load-runner.exe'
$output = Join-Path $root "target\installer\CrestronLoadRunner-$version.msi"
New-Item -ItemType Directory -Force (Split-Path $output) | Out-Null

wix build installer\crestron-load-runner.wxs -arch x64 -d "Version=$version" -d "Exe=$exe" -o $output
if ($LASTEXITCODE -ne 0) { throw 'wix build failed' }

Write-Host "Built $output"
