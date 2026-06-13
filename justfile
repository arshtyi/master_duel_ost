assetstudio := 'C:\Software\AssetStudio\AssetStudioModCLI.exe'
game_dir := 'C:\Software\Steam\steamapps\common\Yu-Gi-Oh!  Master Duel'
export_dir := 'Microsoft.PowerShell.Core\FileSystem::\\wsl.localhost\FedoraLinux-44\home\arshtyi\workspace\master_duel_ost'

default: list

list:
    @just --list

export-ost:
    powershell.exe -NoLogo -NoProfile -NonInteractive -Command '$ErrorActionPreference = "Stop"; $env:DOTNET_ROOT = "C:\Program Files\dotnet"; $output = Convert-Path -LiteralPath "{{ export_dir }}"; & "{{ assetstudio }}" "{{ game_dir }}" -m export -t audio -g type -f assetName -o $output -r --audio-format wav --filter-by-name "^BGM_" --filter-with-regex; if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }'

run:
    cargo run --release
