#!/bin/bash

# TuiMu build cache cleanup.
# Only removes artifacts owned by this repository; it never touches global
# Cargo, mpv, or yt-dlp caches.

set -e

echo "=== TuiMu Cache Cleanup ==="

if [ ! -f "Cargo.toml" ] || [ ! -f "src/main.rs" ]; then
    echo "Error: This script must be run from the TuiMu directory"
    exit 1
fi

echo "This will remove this repository's ./target build directory. Continue? (y/N)"
read -r response
if [[ ! "$response" =~ ^[Yy]$ ]]; then
    echo "Cancelled."
    exit 0
fi

cargo clean

echo "=== Cleanup complete ==="
echo "You can now build TuiMu with:"
echo "  cargo build --release"
