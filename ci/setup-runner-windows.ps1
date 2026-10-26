# Sets up a Gitea Actions runner with the "windows" label on this PC.
# Run in an elevated PowerShell (it asks for the runner registration token, which so stays out of the history):
#   powershell -ExecutionPolicy Bypass -File setup-runner-windows.ps1 -Instance https://gitea.example.com
# Token: Gitea -> repository Settings -> Actions -> Runners -> Create new runner (a repository runner only takes
# this repository's jobs). Regenerate the token in Gitea once the runner is registered.
# The runner runs jobs as the user who runs this script, with that user's files and rights: better a standard
# (non-administrator) account, and only jobs from repositories you control.
param(
    [Parameter(Mandatory)][string]$Instance,
    [string]$Version = "4.1.0",
    [string]$Dir = "C:\gitea-runner"
)
$ErrorActionPreference = "Stop"

# Job tools: Git, PowerShell 7 (default job shell on Windows), Rust (MSVC) and the
# VS C++ build tools (hidapi compiles C code; also needed by OpenDeck's MSVC target).
$winget = "--exact", "--accept-package-agreements", "--accept-source-agreements"
winget install --id Git.Git @winget
winget install --id Microsoft.PowerShell @winget
winget install --id Microsoft.VisualStudio.2022.BuildTools @winget --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --includeRecommended"
# the Windows ARM64 build (cross-compiled here): ring's assembly needs clang
winget install --id LLVM.LLVM @winget
winget install --id Rustlang.Rustup @winget
$env:Path += ";$env:USERPROFILE\.cargo\bin"
rustup default stable
rustup component add clippy
rustup target add aarch64-pc-windows-msvc

# Runner
New-Item -ItemType Directory -Force $Dir | Out-Null
Set-Location $Dir
$url = "https://dl.gitea.com/gitea-runner/$Version/gitea-runner-$Version-windows-amd64.exe"
Invoke-WebRequest $url -OutFile gitea-runner.exe
# the checksum Gitea publishes next to the download
$expected = ((Invoke-WebRequest "$url.sha256").Content -split '\s+')[0]
if ((Get-FileHash gitea-runner.exe -Algorithm SHA256).Hash -ne $expected) { Remove-Item gitea-runner.exe; throw 'checksum mismatch' }
$secure = Read-Host 'Runner registration token' -AsSecureString
$token = [Net.NetworkCredential]::new('', $secure).Password
.\gitea-runner.exe register --no-interactive --instance $Instance --token $token --name "$env:COMPUTERNAME-windows" --labels "windows:host"
if ($LASTEXITCODE) { throw 'registration failed' }

# Start at boot as the current user (jobs need this user's Rust setup), without a window
# and without storing a password (S4U logon).
$action = New-ScheduledTaskAction -Execute "$Dir\gitea-runner.exe" -Argument "daemon" -WorkingDirectory $Dir
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType S4U -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -StartWhenAvailable
Register-ScheduledTask -TaskName "Gitea Runner" -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName "Gitea Runner"
"Runner registered and started. Check: Gitea -> Settings -> Actions -> Runners (should be Idle). Now regenerate the registration token in Gitea."
