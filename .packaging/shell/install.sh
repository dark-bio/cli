#!/bin/sh
set -eu

version='@VERSION@'
release="https://github.com/dark-bio/cli/releases/download/v$version"
case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) platform=macos-arm64 ;;
    Darwin/x86_64) platform=macos-amd64 ;;
    Linux/aarch64|Linux/arm64) platform=linux-arm64 ;;
    Linux/x86_64) platform=linux-amd64 ;;
    *) echo 'error: unsupported platform' >&2; exit 1 ;;
esac

temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
trap 'exit 1' HUP INT TERM

curl --fail --location --silent --show-error --retry 3 \
    "$release/ark-$version-$platform" \
    --output "$temporary/ark"
curl --fail --location --silent --show-error --retry 3 \
    "$release/LICENSES.txt" --output "$temporary/LICENSES.txt"

directory="$HOME/.local/bin"
notices="$HOME/.local/share/ark"
mkdir -p "$directory" "$notices"
install -m 644 "$temporary/LICENSES.txt" "$notices/LICENSES.txt"
install -m 755 "$temporary/ark" "$directory/ark"
printf 'Installed ark %s to %s/ark\n' "$version" "$directory"
case ":$PATH:" in
    *":$directory:"*) ;;
    *) printf 'Add %s to your PATH to run ark.\n' "$directory" ;;
esac
