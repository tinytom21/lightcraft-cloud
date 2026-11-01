# Tests the Windows installer on a real Windows (CI runs this; see .github/workflows/release.yml). It installs for the current user, so
# run it only on a machine you do not mind: a throwaway runner. It touches the real default places (it is the point of the test).
#
#   test-installer.ps1 -Setup <lightcraft-cloud-x.y.z-windows-setup.exe> -Version x.y.z
#
# What it checks: a silent install to the default place; the version, from there; the Run value; the Start menu entry and the uninstall
# entry; the installer run again over a RUNNING copy (the app is asked to quit, files replaced, the app started again); `quit`; that a Run value
# the user turned off stays off and one that points elsewhere (the zip) is moved here; a silent uninstall with the app running; and that a
# planted config, state, rclone config and photo file all survive.
# What it cannot check: a runner has no desktop session, so there is no tray icon to look at, no Start menu to click and no window.
param(
    [Parameter(Mandatory)][string]$Setup,
    [Parameter(Mandatory)][string]$Version
)
$ErrorActionPreference = 'Stop'

$logs = Join-Path (Get-Location) 'ci-logs'
New-Item -ItemType Directory -Force $logs | Out-Null
$app = Join-Path $env:LOCALAPPDATA 'Programs\LightCraft Cloud'
$exe = Join-Path $app 'lightcraft-cloud-desktop.exe'
$cfg = Join-Path $env:APPDATA 'lightcraft-cloud-desktop'
$state = Join-Path $env:LOCALAPPDATA 'lightcraft-cloud-desktop'
$rclone = Join-Path $env:APPDATA 'rclone'
$photos = 'C:\lc-ci-photos'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$runName = 'LightCraft Cloud'
$uninstKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{AD15624E-CC27-4CAD-9106-87BE24794BB7}_is1'
$lnk = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\LightCraft Cloud.lnk'
$appLog = Join-Path $state 'logs\lightcraft-cloud-desktop.log'

function Step($m) { Write-Host ""; Write-Host "=== $m" }
function Check($cond, $msg) { if (-not $cond) { throw "FAILED: $msg" } else { Write-Host "ok: $msg" } }
function RunValue { (Get-ItemProperty -Path $runKey -Name $runName -ErrorAction SilentlyContinue).$runName }
function AppProcs { @(Get-Process -Name 'lightcraft-cloud-desktop' -ErrorAction SilentlyContinue) }
function AppLog { if (Test-Path $appLog) { @(Get-Content $appLog) } else { @() } }
function Count($lines, $text) { @($lines | Where-Object { $_ -like "*$text" }).Count }
function WaitFor([scriptblock]$cond, [int]$secs) {
    $end = (Get-Date).AddSeconds($secs)
    while ((Get-Date) -lt $end) { if (& $cond) { return $true }; Start-Sleep -Milliseconds 300 }
    return $false
}
# (Start-Process -Wait also waits for every process the program started, and the installer starts the app and leaves it running: so wait for this process only)
function RunAndWait($file, $arguments) {
    $p = Start-Process -FilePath $file -ArgumentList $arguments -PassThru
    if (-not $p.WaitForExit(300000)) { $p.Kill(); throw "FAILED: $file did not finish in five minutes" }
    return $p.ExitCode
}
function RunSetup($tag) {
    $log = Join-Path $logs "setup-$tag.log"
    $code = RunAndWait $Setup @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$log`"")
    Write-Host "setup ($tag) exit code $code"
    return $code
}
function StartApp {
    $p = Start-Process -FilePath $exe -ArgumentList 'run', '--headless' -PassThru
    $n = Count (AppLog) 'app started'
    Check (WaitFor { (Count (AppLog) 'app started') -gt $n } 30) "the app (pid $($p.Id)) started"
    return $p
}

# ---------------------------------------------------------------- what the user already has
Step 'plant the user''s own files (they must survive everything below)'
New-Item -ItemType Directory -Force $cfg, $state, $rclone, $photos | Out-Null
Set-Content -Path (Join-Path $cfg 'config.toml') -Value "check_for_updates = false`nnotify = false`n"
Set-Content -Path (Join-Path $cfg 'planted-config.txt') -Value 'planted config'
Set-Content -Path (Join-Path $state 'planted-state.txt') -Value 'planted state'
Set-Content -Path (Join-Path $rclone 'planted-rclone.conf') -Value '[x]'
Set-Content -Path (Join-Path $photos 'IMG_TEST.txt') -Value 'not a photo'
Check (-not (Test-Path $exe)) 'nothing is installed yet'

# ---------------------------------------------------------------- a first install
Step 'silent install to the default place'
Check ((RunSetup 'first') -eq 0) 'the installer exits with 0'
foreach ($f in 'lightcraft-cloud-desktop.exe', 'librclone.dll', 'README.txt', 'LICENSE-MIT.txt', 'LICENSE-APACHE.txt', 'LICENSE-rclone.txt', 'LICENSE-Inter-OFL.txt', 'unins000.exe') {
    Check (Test-Path (Join-Path $app $f)) "$f is in $app"
}
$v = & $exe --version | Out-String
Write-Host $v
Check ($v -match "lightcraft-cloud-desktop $([regex]::Escape($Version))") "--version from the installed exe says $Version"
Check ((RunValue) -eq "`"$exe`" run") "the Run value is the quoted exe and 'run': $(RunValue)"
Check (Test-Path $lnk) 'the Start menu entry exists'
$u = Get-ItemProperty $uninstKey
Check ($u.DisplayVersion -eq $Version -and $u.DisplayName -like 'LightCraft Cloud*') "Apps & features lists '$($u.DisplayName)' $($u.DisplayVersion)"
Check ((AppProcs).Count -eq 0) 'the first install starts nothing in a silent run'

# ---------------------------------------------------------------- the installer run over a running copy
Step 'install again over a running copy'
$old = StartApp
$stopsBefore = Count (AppLog) 'app stopped'
Check ((RunSetup 'over-running') -eq 0) 'the installer exits with 0'
Check ($old.HasExited) "the old process (pid $($old.Id)) is gone"
Check ((Count (AppLog) 'app stopped') -gt $stopsBefore) 'it stopped by itself (it wrote "app stopped": it was asked to quit, not killed)'
Check (WaitFor { (AppProcs | Where-Object { $_.Id -ne $old.Id }).Count -gt 0 } 30) 'the installer started the app again'
$new = AppProcs | Where-Object { $_.Id -ne $old.Id } | Select-Object -First 1
Check ($new.Path -eq $exe) "the new process runs from the install folder (pid $($new.Id))"
Start-Sleep -Seconds 5
Check (-not $new.HasExited) 'and it stays up (no tray session here, so it runs without an icon)'
Check ((RunValue) -eq "`"$exe`" run") 'the Run value is still right'

Step 'quit'
$code = RunAndWait $exe @('quit', '--wait', '30')
Check ($code -eq 0) "quit stopped the app (exit code $code)"
Check ((AppProcs).Count -eq 0) 'no process is left'
$code = RunAndWait $exe @('quit')
Check ($code -eq 10) "quit with no app running says so (exit code $code)"

# ---------------------------------------------------------------- the Run value
Step 'a Run value the user turned off stays off'
Remove-ItemProperty -Path $runKey -Name $runName
Check ((RunSetup 'value-off') -eq 0) 'the installer exits with 0'
Check ($null -eq (RunValue)) 'the Run value did not come back'

Step 'a Run value that points at the zip copy is moved to the installed one'
New-ItemProperty -Path $runKey -Name $runName -Value '"C:\old zip folder\lightcraft-cloud-desktop.exe" run' -PropertyType String -Force | Out-Null
Check ((RunSetup 'from-zip') -eq 0) 'the installer exits with 0'
Check ((RunValue) -eq "`"$exe`" run") "it now points at $exe"

# ---------------------------------------------------------------- uninstall
Step 'silent uninstall with the app running'
$running = StartApp
$un = Join-Path $app 'unins000.exe'
$ulog = Join-Path $logs 'uninstall.log'
$code = RunAndWait $un @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', "/LOG=`"$ulog`"")
Write-Host "uninstaller exit code $code"
# (the uninstaller copies itself and returns at once; the work is done by the copy)
Check (WaitFor { -not (Test-Path $exe) } 90) 'the program files are gone'
Check (WaitFor { (AppProcs).Count -eq 0 } 30) 'the app was stopped'
Check ($running.HasExited) 'it was the one that ran'
Check ($null -eq (RunValue)) 'the Run value is gone'
Check (-not (Test-Path $uninstKey)) 'the Apps & features entry is gone'
Check (-not (Test-Path $lnk)) 'the Start menu entry is gone'
Check (WaitFor { -not (Test-Path (Join-Path $app 'librclone.dll')) } 30) 'librclone.dll is gone'

Step 'the user''s own files survived'
Check ((Get-Content (Join-Path $cfg 'config.toml') -Raw) -match 'check_for_updates') 'config.toml'
Check ((Get-Content (Join-Path $cfg 'planted-config.txt')) -eq 'planted config') 'the planted file in the config folder'
Check ((Get-Content (Join-Path $state 'planted-state.txt')) -eq 'planted state') 'the planted file in the state folder'
Check (Test-Path (Join-Path $state 'logs\lightcraft-cloud-desktop.log')) 'the log'
Check (Test-Path (Join-Path $rclone 'planted-rclone.conf')) 'the rclone config folder'
Check ((Get-Content (Join-Path $photos 'IMG_TEST.txt')) -eq 'not a photo') 'the photo folder'

Write-Host ""
Write-Host "ALL INSTALLER CHECKS PASSED"
