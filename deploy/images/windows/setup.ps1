# Finishes a Windows image for metastratum (build.sh), run once as the
# build signs in: the virtio drivers' services and QEMU's guest agent, then
# cloudbase-init (which takes the VM's hostname, password and user data from
# its cloud-init seed), Remote Desktop, the serial console (SAC), and the
# image made small and generalized. Sysprep shuts the VM down when done.
$ErrorActionPreference = 'Stop'
Start-Transcript -Path C:\Windows\Temp\mst-image-setup.log

$build = (Get-Volume -FileSystemLabel MSTBUILD).DriveLetter + ':'
$virtio = Get-Volume | Where-Object { $_.DriveLetter -and (Test-Path ($_.DriveLetter + ':\virtio-win-gt-x64.msi')) } |
    Select-Object -First 1 | ForEach-Object { $_.DriveLetter + ':' }
if (-not $virtio) { throw 'the virtio-win disc is missing' }

function Install-Msi([string] $path, [string[]] $properties = @()) {
    $p = Start-Process msiexec.exe -Wait -PassThru -ArgumentList (@('/i', "`"$path`"", '/qn', '/norestart') + $properties)
    # 3010: installed, reboot required (sysprep reboots anyway).
    if ($p.ExitCode -notin 0, 3010) { throw "installing $path failed: $($p.ExitCode)" }
}

# Drivers (balloon, serial, ...) and their services, and the guest agent the
# controller asks whether the first boot is done.
Install-Msi "$virtio\virtio-win-gt-x64.msi" @('ADDLOCAL=ALL')
Install-Msi "$virtio\guest-agent\qemu-ga-x86_64.msi"

Install-Msi "$build\CloudbaseInitSetup_x64.msi" @('RUN_SERVICE_AS_LOCAL_SYSTEM=1')
$cbi = "$env:ProgramFiles\Cloudbase Solutions\Cloudbase-Init"
Copy-Item "$build\cloudbase-init.conf" "$cbi\conf\cloudbase-init.conf" -Force
Copy-Item "$build\cloudbase-init-unattend.conf" "$cbi\conf\cloudbase-init-unattend.conf" -Force
# Not while a new VM specializes (see sysprep-unattend.xml, which starts it).
Set-Service cloudbase-init -StartupType Manual

# The VM's clock is UTC (as Linux guests', so they migrate alike).
Set-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Control\TimeZoneInformation RealTimeIsUniversal -Type DWord -Value 1

# Remote Desktop, and ping.
Set-ItemProperty 'HKLM:\System\CurrentControlSet\Control\Terminal Server' fDenyTSConnections -Type DWord -Value 0
Enable-NetFirewallRule -DisplayGroup 'Remote Desktop'
Enable-NetFirewallRule -Name FPS-ICMP4-ERQ-In

# The serial console: Special Administration Console (SAC) on COM1.
bcdedit /ems '{current}' on | Out-Null
bcdedit /emssettings EMSPORT:1 EMSBAUDRATE:115200 | Out-Null

# Small: superseded components gone, free space handed back to the disk.
Dism.exe /Online /Cleanup-Image /StartComponentCleanup /ResetBase | Out-Null
Optimize-Volume -DriveLetter C -ReTrim

Stop-Transcript
# Generalized, it starts afresh on each VM, where cloudbase-init sets it up.
Copy-Item "$build\sysprep-unattend.xml" "$cbi\conf\Unattend.xml" -Force
& "$env:SystemRoot\System32\Sysprep\sysprep.exe" /generalize /oobe /shutdown /quiet "/unattend:$cbi\conf\Unattend.xml"
