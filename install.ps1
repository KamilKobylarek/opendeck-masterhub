# Builds the plugin and installs it into OpenDeck (Windows). Restart OpenDeck afterwards.
# Run: powershell -ExecutionPolicy Bypass -File install.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
cargo build --release
if ($LASTEXITCODE) { exit $LASTEXITCODE }
$plugin = "pl.kobylarek.masterhub.sdPlugin"
$triple = (rustc -vV | Select-String "^host: (.+)").Matches[0].Groups[1].Value # must match manifest.json CodePaths
New-Item -ItemType Directory -Force "$plugin\bin\$triple" | Out-Null
Copy-Item "target\release\opendeck-masterhub.exe" "$plugin\bin\$triple\" -Force
$dest = "$env:APPDATA\opendeck\plugins"
New-Item -ItemType Directory -Force $dest | Out-Null
# the plugin's folder before 0.2.0, under its old id: both would claim the same devices
foreach ($old in @("$dest\$plugin", "$dest\com.masterhub.opendeck.sdPlugin") | Where-Object { Test-Path $_ }) {
  try { Remove-Item -Recurse -Force $old }
  catch { throw "Can't replace the installed plugin while OpenDeck runs it: quit OpenDeck and run this again (or install the release zip with OpenDeck -> Plugins -> Install from file)." }
}
Copy-Item -Recurse $plugin $dest
"Installed to $dest\$plugin"
