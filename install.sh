#!/bin/bash

# Tuimu Installation Script for Linux
# This script installs tuimu without requiring Cargo

set -e

echo "=== Tuimu Installation Script ==="

# Check if running as root
if [ "$EUID" -eq 0 ]; then
    echo "Please do not run this script as root"
    exit 1
fi

# Check if we have the required tools
echo "Checking required tools..."

# Check if curl is installed
if ! command -v curl &> /dev/null; then
    echo "Error: curl is not installed. Please install it first:"
    echo "  Ubuntu/Debian: sudo apt-get install curl"
    echo "  Fedora: sudo dnf install curl"
    echo "  Arch: sudo pacman -S curl"
    exit 1
fi

# Get system architecture
ARCH=$(uname -m)
case $ARCH in
    x86_64) ARCH="x86_64" ;;
    aarch64) ARCH="aarch64" ;;
    armv7l) ARCH="armv7l" ;;
    *) 
        echo "Error: Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

# Get latest version from GitHub
echo "Fetching latest version information..."
LATEST_VERSION=$(curl -s "https://api.github.com/repos/masardon/tuimu/releases/latest" | grep -oP '"tag_name": "v\K[^"]+' || echo "latest")
echo "Latest version: $LATEST_VERSION"

# Download URL
DOWNLOAD_URL="https://github.com/masardon/tuimu/releases/download/$LATEST_VERSION/tuimu-$LATEST_VERSION-linux-$ARCH.tar.gz"

# Create temporary directory
TEMP_DIR=$(mktemp -d)
cd "$TEMP_DIR"

echo "Downloading tuimu..."
curl -L -o tuimu.tar.gz "$DOWNLOAD_URL"

echo "Extracting..."
tar -xzf tuimu.tar.gz

# Find the binary
BINARY=$(find . -name "tuimu" -type f)
if [ -z "$BINARY" ]; then
    echo "Error: Could not find tuimu binary"
    exit 1
fi

# Make it executable
chmod +x "$BINARY"

# Install to /usr/local/bin
echo "Installing to /usr/local/bin..."
sudo mv "$BINARY" /usr/local/bin/tuimu

# Clean up
cd /
rm -rf "$TEMP_DIR"

echo "=== Installation complete ==="
echo "tuimu has been installed to /usr/local/bin/tuimu"
echo ""
echo "You can now run it with:"
echo "  tuimu"
echo ""
echo "For usage instructions, see the README.md file"
echo ""
echo "To uninstall, run:"
echo "  sudo rm /usr/local/bin/tuimu"