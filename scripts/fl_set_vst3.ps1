param([string]$Path = 'D:\PluginVault\bridges')

$list = 'HKCU:\Software\Image-Line\Shared\Plugin search paths\List'
$vst3 = 7

$running = Get-Process FL64, FL, PluginManager -ErrorAction SilentlyContinue
if ($running) {
    Write-Output "Close FL Studio and its Plugin Manager first: $(($running | ForEach-Object { $_.Name }) -join ', ')"
    exit 1
}

$wanted = $Path.TrimEnd('\')
$entries = @(Get-ChildItem $list -ErrorAction SilentlyContinue | Where-Object { (Get-ItemProperty $_.PSPath).path.TrimEnd('\') -ieq $wanted })
if ($entries.Count -eq 0) {
    Write-Output "$Path is not in FL Studio's plugin search paths. Add it in FL first, then run this again."
    exit 1
}
foreach ($entry in $entries) {
    $before = (Get-ItemProperty $entry.PSPath).plugclass
    Set-ItemProperty $entry.PSPath -Name plugclass -Value $vst3 -Type DWord
    Write-Output "entry $($entry.PSChildName): $Path plugclass $before -> $vst3 (VST3)"
}
Write-Output 'Open FL Studio and run Find installed plugins.'
