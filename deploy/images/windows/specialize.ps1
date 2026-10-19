# Run by sysprep as a new VM specializes (sysprep-unattend.xml), before
# cloudbase-init.
#
# The built-in Administrator. On Windows Server, sysprep leaves its password
# blank, to be changed at the first sign-in (the sign-in screen would ask for
# that before the VM's is set, or, not to be changed, sign in by itself): it
# gets a random one, known to no one, until cloudbase-init sets the VM's
# (when it is the VM's account). On Windows 11, which does not use it, it is
# switched off (the build signed in with it).
#
# Smart App Control (Windows 11), unless the image keeps Windows' default
# (build.sh --keep-smart-app-control): off, so that no VM later starts
# blocking software on its own (it switches itself on or off after watching
# what runs; off, it stays off).
$client = (Get-CimInstance Win32_OperatingSystem).ProductType -eq 1
if ($client) {
    net.exe user Administrator /active:no | Out-Null
} else {
    $p = -join ((48..57) + (65..90) + (97..122) | Get-Random -Count 32 | ForEach-Object { [char]$_ }) + 'aA1!'
    net.exe user Administrator $p /logonpasswordchg:no | Out-Null
}
if ($client -and (Test-Path "$PSScriptRoot\smart-app-control-off")) {
    Set-ItemProperty HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy VerifiedAndReputablePolicyState -Type DWord -Value 0
}
