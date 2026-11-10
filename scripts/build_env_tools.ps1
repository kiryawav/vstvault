$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
cargo build --release -p vault-env
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build --release -p vault-env --target i686-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
$x86 = Join-Path $root 'target\release\x86'
New-Item -ItemType Directory -Force -Path $x86 | Out-Null
foreach ($file in 'vault-run.exe', 'vault_env.dll') {
    Copy-Item (Join-Path $root "target\i686-pc-windows-msvc\release\$file") $x86 -Force
}
