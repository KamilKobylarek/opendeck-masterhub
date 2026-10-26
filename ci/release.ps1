# Windows part of a release (see .gitea/workflows/release.yml), in PowerShell 7:
#   release.ps1 build    builds x64 and ARM64 and packs dist\opendeck-masterhub-windows.zip
#   release.ps1 upload   adds it to the Gitea release of the pushed tag (a pre-release until it's complete),
#                        replacing an asset of the same name; needs the RELEASE_TOKEN secret
param([Parameter(Mandatory)][ValidateSet('build', 'upload')][string]$Step)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot)
$tag = $env:GITHUB_REF_NAME
if ($tag -notmatch '^v\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$') { throw "tag '$tag' is not a version like v1.2.3" }
$plugin = 'com.masterhub.opendeck.sdPlugin'
$zip = 'dist\opendeck-masterhub-windows.zip'

if ($Step -eq 'build') {
  # x64 and ARM64, cross-compiled on the x64 runner. ARM64 needs the VS C++ ARM64 build tools (hidapi is C) and
  # LLVM's clang (ring's assembly), both installed by ci/setup-runner-windows.ps1.
  $llvm = Join-Path $env:ProgramFiles 'LLVM\bin'
  if (Test-Path $llvm) { $env:Path = "$llvm;$env:Path" }
  foreach ($triple in 'x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc') {
    rustup target add $triple
    if ($LASTEXITCODE) { throw "rustup target add $triple failed" }
    cargo build --release --locked --target $triple
    if ($LASTEXITCODE) { throw "cargo build for $triple failed" }
    New-Item -ItemType Directory -Force "$plugin\bin\$triple" | Out-Null
    Copy-Item -Force "target\$triple\release\opendeck-masterhub.exe" "$plugin\bin\$triple\"
  }

  $stage = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid())
  New-Item -ItemType Directory $stage | Out-Null
  Copy-Item -Recurse $plugin $stage
  $manifest = "$stage\$plugin\manifest.json"
  $version = $tag -replace '^v', ''
  $text = [IO.File]::ReadAllText($manifest) -replace '"Version": "[^"]*"', "`"Version`": `"$version`""
  [IO.File]::WriteAllText($manifest, $text, [Text.UTF8Encoding]::new($false)) # UTF-8 without a BOM
  New-Item -ItemType Directory -Force dist | Out-Null
  # tar (bsdtar, in Windows 10 and later) writes '/' in the zip's paths; Compress-Archive may write '\', which
  # other systems and OpenDeck unpack as odd file names
  if (Test-Path $zip) { Remove-Item $zip }
  tar -a -c -f $zip -C $stage $plugin
  if ($LASTEXITCODE) { throw 'tar failed' }
  Remove-Item -Recurse $stage
  "packed $zip"
  exit 0
}

if (-not $env:RELEASE_TOKEN) { throw 'set the RELEASE_TOKEN secret (Gitea access token, repository write)' }
$api = "$env:GITHUB_SERVER_URL/api/v1/repos/$env:GITHUB_REPOSITORY"
$headers = @{ Authorization = "token $env:RELEASE_TOKEN" }
$byTag = "$api/releases/tags/$([Uri]::EscapeDataString($tag))"
try { $release = Invoke-RestMethod -Headers $headers $byTag }
catch {
  if ($_.Exception.Response.StatusCode.value__ -ne 404) { throw }
  try { $release = Invoke-RestMethod -Method Post -Headers $headers -ContentType 'application/json' -Body (@{ tag_name = $tag; name = $tag; prerelease = $true } | ConvertTo-Json) "$api/releases" }
  catch { $release = Invoke-RestMethod -Headers $headers $byTag } # another job made it
}
$name = Split-Path -Leaf $zip
foreach ($asset in $release.assets | Where-Object name -eq $name) { # a re-run: replace, don't add a second one
  Invoke-RestMethod -Method Delete -Headers $headers "$api/releases/$($release.id)/assets/$($asset.id)" | Out-Null
}
Invoke-RestMethod -Method Post -Headers $headers -Form @{ attachment = Get-Item $zip } "$api/releases/$($release.id)/assets?name=$name" | Out-Null
"uploaded $name to release $tag"
