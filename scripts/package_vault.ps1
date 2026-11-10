param([Parameter(Mandatory = $true)][string]$Vault)

$root = Split-Path -Parent $PSScriptRoot
$rel = Join-Path $root 'target\release'
$tools = Join-Path $Vault 'tools'
New-Item -ItemType Directory -Force -Path (Join-Path $tools 'x86') | Out-Null
foreach ($file in 'vault-cli.exe', 'vault-host.exe', 'vault-run.exe', 'vault_env.dll', 'vault_bridge.dll') {
    Copy-Item (Join-Path $rel $file) $tools -Force
}
foreach ($file in 'vault-run.exe', 'vault_env.dll') {
    Copy-Item (Join-Path $rel "x86\$file") (Join-Path $tools 'x86') -Force
}
Get-ChildItem $tools -Recurse -File | Select-Object @{ n = 'File'; e = { $_.FullName.Substring($tools.Length + 1) } }, Length
