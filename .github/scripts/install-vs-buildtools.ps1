<#
.SYNOPSIS
  Ensure Visual Studio 2022 Build Tools (MSVC + clang/libclang) are present at
  C:\BuildTools on a Windows CI runner, failing fast and loudly if they cannot be.

.DESCRIPTION
  Shared by every Windows job that builds ePHPm (windows-php-check.yml,
  release.yml, release-php-beta.yml, nightly.yml, build-memory.yml), so the
  provisioning logic lives in exactly one place.

  Fast path: the ephemerd `runner-ci-windows-msvc` image (ephpm/ephemerd#186)
  pre-bakes the Build Tools at C:\BuildTools with the same components requested
  below, so on that image this script finds MSBuild.exe and exits in seconds.

  Slow path (a runner on an image WITHOUT the Build Tools): run the official
  bootstrapper. Issue #377 is what this path used to do wrong: it ignored the
  bootstrapper's exit code and then polled for MSBuild.exe for a flat 20
  minutes, so a failed install, a full disk and a dead runner all surfaced as
  the same misleading "timed out" 20 minutes later, with the installer's own
  error never shown. Now:

    * the bootstrapper's exit code is checked, and a failure aborts at once with
      the code's meaning and the tail of the installer logs;
    * the bootstrapper itself is bounded (it can hang on a wedged runner);
    * the post-exit wait for MSBuild.exe (the install engine can outlive the
      bootstrapper on Server Core) only continues while an installer process is
      actually still running — if none is, waiting longer cannot help, so the
      step fails immediately instead of sleeping out the clock;
    * free disk space is checked up front, because a full volume was one of the
      #377 failure signatures and is otherwise invisible until the timeout.
#>
[CmdletBinding()]
param(
    [string]$InstallPath = 'C:\BuildTools',
    # Upper bound on the whole install (bootstrapper + trailing engine work). A
    # healthy install takes ~10-20 minutes; past this the runner is wedged.
    [int]$MaxMinutes = 35,
    # The install needs roughly 7-8 GB; refuse to start below this.
    [int]$MinFreeGB = 12,
    # Overridable only so the failure paths can be exercised locally.
    [string]$BootstrapperUrl = 'https://aka.ms/vs/17/release/vs_buildtools.exe'
)

$ErrorActionPreference = 'Stop'
$msbuild = Join-Path $InstallPath 'MSBuild\Current\Bin\MSBuild.exe'

function Fail([string]$message) {
    Write-Host "::error title=VS Build Tools provisioning failed::$message"
    Write-Host 'This is a RUNNER PROVISIONING failure, not a compile error in the change under test (#377).'
    exit 1
}

function Show-InstallerLogs {
    # The bootstrapper and install engine log to %TEMP% as dd_*.log. Print the
    # tail of the newest few so the real error is in the job log.
    $logs = Get-ChildItem -Path $env:TEMP -Filter 'dd_*.log' -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 3
    if (-not $logs) {
        Write-Host "(no dd_*.log installer logs found under $env:TEMP)"
        return
    }
    foreach ($log in $logs) {
        Write-Host "::group::Installer log tail: $($log.FullName)"
        Get-Content -Path $log.FullName -Tail 60 -ErrorAction SilentlyContinue | ForEach-Object { Write-Host $_ }
        Write-Host '::endgroup::'
    }
}

function Get-InstallerProcesses {
    Get-Process -ErrorAction SilentlyContinue |
        Where-Object { $_.ProcessName -match '^(vs_buildtools|vs_bootstrapper|vs_setup_bootstrapper|vs_installer|vs_installershell|setup)$' }
}

# ---- Fast path: pre-baked runner image -------------------------------------
if (Test-Path $msbuild) {
    Write-Host "VS Build Tools already present at $InstallPath - skipping install"
    exit 0
}

Write-Host "::warning::VS Build Tools not found at $InstallPath - this runner is not on the pre-baked runner-ci-windows-msvc image (ephpm/ephemerd#186); installing now (typically 10-20 minutes)."
if (Test-Path $InstallPath) {
    Write-Host "$InstallPath exists but $msbuild does not (partial or broken install); the bootstrapper will repair it."
}

# ---- Preflight: disk space -------------------------------------------------
$driveName = (Split-Path -Qualifier $InstallPath).TrimEnd(':')
$drive = Get-PSDrive -Name $driveName -ErrorAction SilentlyContinue
if ($drive) {
    $freeGB = [math]::Round($drive.Free / 1GB, 1)
    Write-Host "Free space on ${driveName}: $freeGB GB"
    if ($freeGB -lt $MinFreeGB) {
        Fail "only $freeGB GB free on ${driveName}: (need at least $MinFreeGB GB for the Build Tools install)"
    }
}

# ---- Download the bootstrapper ----------------------------------------------
$tempDir = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$bootstrapper = Join-Path $tempDir 'vs_buildtools.exe'
Write-Host 'Downloading VS Build Tools bootstrapper...'
try {
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -Uri $BootstrapperUrl -OutFile $bootstrapper -UseBasicParsing
} catch {
    Fail "could not download vs_buildtools.exe: $($_.Exception.Message)"
}

# ---- Run it, bounded ----------------------------------------------------------
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$deadlineMs = $MaxMinutes * 60 * 1000
$proc = Start-Process -FilePath $bootstrapper -PassThru -NoNewWindow -ArgumentList @(
    '--quiet', '--wait', '--norestart', '--nocache',
    '--installPath', $InstallPath,
    '--add', 'Microsoft.VisualStudio.Workload.VCTools',
    '--add', 'Microsoft.VisualStudio.Component.VC.Llvm.Clang',
    '--includeRecommended'
)
# Touch the handle now: without it Windows PowerShell can report a $null
# ExitCode for a process started with -PassThru once it has exited.
$null = $proc.Handle

if (-not $proc.WaitForExit($deadlineMs)) {
    Show-InstallerLogs
    try { $proc.Kill() } catch { }
    Fail "the bootstrapper was still running after $MaxMinutes minutes - runner is wedged"
}
$exitCode = $proc.ExitCode
Remove-Item -Path $bootstrapper -ErrorAction SilentlyContinue
Write-Host ("Bootstrapper exited with code {0} after {1:F0}s" -f $exitCode, $sw.Elapsed.TotalSeconds)

# https://learn.microsoft.com/visualstudio/install/use-command-line-parameters-to-install-visual-studio#error-codes
switch ($exitCode) {
    0 { }
    3010 { Write-Host 'Install succeeded; a reboot was requested (ignored on an ephemeral runner).' }
    default {
        $meaning = switch ($exitCode) {
            1 { 'generic install failure' }
            740 { 'elevation required (the runner is not running as administrator)' }
            1001 { 'the Visual Studio installer process is already running' }
            1602 { 'the install was cancelled' }
            1618 { 'another installation is already in progress' }
            1641 { 'a reboot was initiated' }
            5003 { 'the bootstrapper failed to download the installer' }
            5004 { 'the install was cancelled' }
            5007 { 'the install was blocked (not elevated / unsupported OS)' }
            8001 { 'arm64 machine check failed' }
            8002 { 'background download precheck failed' }
            8003 { 'out-of-support selectable' }
            8006 { 'Visual Studio processes are running and block the install' }
            default { 'see the installer log tail above' }
        }
        Show-InstallerLogs
        Fail "vs_buildtools.exe exited with code $exitCode ($meaning)"
    }
}

# ---- Wait for the engine to finish, only while it is actually running --------
# On Server Core the bootstrapper can return before the install engine has laid
# MSBuild down. Keep waiting only while an installer process is alive; once
# none is (for a short grace period) the install is over, and if MSBuild is
# still missing it failed.
$idleSince = $null
while (-not (Test-Path $msbuild)) {
    if ($sw.Elapsed.TotalMilliseconds -ge $deadlineMs) {
        Show-InstallerLogs
        Fail "MSBuild.exe still missing after $MaxMinutes minutes"
    }
    if (Get-InstallerProcesses) {
        $idleSince = $null
    } elseif (-not $idleSince) {
        $idleSince = [System.Diagnostics.Stopwatch]::StartNew()
    } elseif ($idleSince.Elapsed.TotalSeconds -ge 60) {
        Show-InstallerLogs
        Fail "the installer reported success (exit $exitCode) but $msbuild was never created and no installer process is running"
    }
    Write-Host ("  ...waiting for the install engine to finish ({0:F0}s)" -f $sw.Elapsed.TotalSeconds)
    Start-Sleep -Seconds 10
}

Write-Host ("MSBuild.exe found after {0:F0}s" -f $sw.Elapsed.TotalSeconds)
exit 0
