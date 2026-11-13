# Derive the Raycast device_id (X-Raycast-DeviceId) on Windows.
#
# Reversed from the official client's `backend/index.mjs` (`Tsn`):
#
#   device_id = SHA256(uuid + identifyingNumber + SALT + "Production")
#
# where `uuid` and `identifyingNumber` come from WMI
# `Win32_ComputerSystemProduct` (uppercase, hyphenated, concatenated verbatim).
# Verified against a real captured request.
#
# Usage:  pwsh ./get_device_id.ps1

$ErrorActionPreference = 'Stop'

$Salt = 'xK7mQ2vLpN8wY4jR6tBfHsAeDc'

$system = Get-CimInstance -ClassName Win32_ComputerSystemProduct |
    Select-Object -Property UUID, IdentifyingNumber

if (-not $system.UUID -or -not $system.IdentifyingNumber) {
    Write-Error "Win32_ComputerSystemProduct returned no UUID/IdentifyingNumber on this machine"
    exit 1
}

$input = "$($system.UUID)$($system.IdentifyingNumber)$Salt Production".Replace(' ', '')
$sha = [System.Security.Cryptography.SHA256]::Create()
$hash = $sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes($input))
$deviceId = ($hash | ForEach-Object { $_.ToString('x2') }) -join ''

Write-Host "uuid:               $($system.UUID)"
Write-Host "identifyingNumber:  $($system.IdentifyingNumber)"
Write-Host ""
Write-Host "device_id: $deviceId"
