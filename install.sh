#!/bin/bash

# Tuimu Installation Script for Linux
# Installs a pre-built release binary without requiring Cargo.

set -e

echo "=== Tuimu Installation Script ==="

if [ "$EUID" -eq 0 ]; then
    echo "Please do not run this script as root"
    exit 1
fi

if ! command -v curl &> /dev/null; then
    echo "Error: curl is not installed. Please install it first:"
    echo "  Ubuntu/Debian: sudo apt-get install curl"
    echo "  Fedora: sudo dnf install curl"
    echo "  Arch: sudo pacman -S curl"
    exit 1
fi

if ! command -v tar &> /dev/null; then
    echo "Error: tar is not installed."
    exit 1
fi

ARCH=$(uname -m)
case $ARCH in
    x86_64 | aarch64 | armv7l) ;;
    *)
        echo "Error: Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

DOWNLOAD_URL="https://github.com/masardon/tuimu/releases/latest/download/tuimu-linux-$ARCH.tar.gz"

TEMP_DIR=$(mktemp -d)
trap 'rm -rf "$TEMP_DIR"' EXIT
cd "$TEMP_DIR"

echo "Downloading tuimu for $ARCH..."
if ! curl -fL -o tuimu.tar.gz "$DOWNLOAD_URL"; then
    echo "Error: Could not download $DOWNLOAD_URL"
    echo "Check https://github.com/masardon/tuimu/releases for an available build."
    exit 1
fi

echo "Extracting..."
tar -xzf tuimu.tar.gz

BINARY=$(find . -type f -name tuimu -print -quit)
if [ -z "$BINARY" ]; then
    echo "Error: Could not find tuimu binary in the archive"
    exit 1
fi

echo "Installing to /usr/local/bin..."
sudo install -m 0755 "$BINARY" /usr/local/bin/tuimu

echo "=== Installation complete ==="
echo "tuimu has been installed to /usr/local/bin/tuimu"
echo ""

MISSING=0
for dep in yt-dlp mpv curl; do
    if ! command -v "$dep" &> /dev/null; then
        echo "Warning: '$dep' was not found in PATH; tuimu needs it at runtime."
        MISSING=1
    fi
done
if [ "$MISSING" -eq 1 ]; then
    echo ""
    echo "Install missing dependencies, then run 'tuimu'."
else
    echo "Run it with: tuimu"
fi
