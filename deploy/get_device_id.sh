#!/usr/bin/env bash
# Derive the Raycast device_id (X-Raycast-DeviceId) on macOS.
#
# Reversed from the official Windows client's `backend/index.mjs` (`Tsn`):
#
#   device_id = SHA256(uuid + identifyingNumber + SALT + "Production")
#
# On macOS the closest equivalents of the WMI `Win32_ComputerSystemProduct`
# fields are read via `ioreg`/`system_profiler`:
#   - uuid  := IOPlatformUUID
#   - identifyingNumber := the board serial number
#
# NOTE: the SALT and the `Production` suffix were extracted from the Windows
# client. They are most likely shared across platforms (the JS is packaged
# from one codebase), but verify once against a real macOS capture before
# trusting the output: run this script, then compare with the
# `X-Raycast-DeviceId` header of a request captured from the local client.
#
# Usage:  ./get_device_id.sh

set -euo pipefail

SALT='xK7mQ2vLpN8wY4jR6tBfHsAeDc'

uuid=$(/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice |
    awk '/IOPlatformUUID/ { gsub(/"/, ""); print $NF; exit }')

serial=$(/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice |
    awk '/IOPlatformSerialNumber/ { gsub(/"/, ""); print $NF; exit }')

if [[ -z "$uuid" || -z "$serial" ]]; then
    echo "error: could not read IOPlatformUUID/IOPlatformSerialNumber" >&2
    exit 1
fi

device_id=$(printf '%s' "${uuid}${serial}${SALT}Production" | shasum -a 256 | awk '{print $1}')

echo "uuid:               $uuid"
echo "identifyingNumber:  $serial"
echo
echo "device_id: $device_id"
