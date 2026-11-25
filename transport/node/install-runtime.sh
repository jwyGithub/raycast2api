#!/bin/sh
# Fixed official Linux artifacts. Used only in the Docker build stage.
set -eu
destination=${1:?destination required}
architecture=${2:?Docker TARGETARCH required}
case "$architecture" in
  amd64) archive=node-v26.13.1-linux-x64.tar.xz; checksum=5c063e46bab0fbc1c17cc445d7ff129cbdcd0471be6ed83b6ab864f8143bc864 ;;
  arm64) archive=node-v26.13.1-linux-arm64.tar.xz; checksum=89e1853138db51e8b82d3e98ba9c0d3f2956a28e5e55ea1b7e12d25aae92d5dd ;;
  *) echo "Unsupported Node architecture: $architecture" >&2; exit 1 ;;
esac
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
curl --fail --silent --show-error --location "https://nodejs.org/dist/v26.13.1/$archive" --output "$temporary/$archive"
printf '%s  %s\n' "$checksum" "$temporary/$archive" | sha256sum --check --status
mkdir -p "$destination"
tar -xJf "$temporary/$archive" --strip-components=1 -C "$destination"
