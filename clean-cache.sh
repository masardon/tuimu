#!/bin/bash

# Tuimu Cache Cleanup Script
# This script helps clean up caches before building or releasing tuimu

set -e

echo "=== Tuimu Cache Cleanup ==="

# Check if we're in the tuimu directory
if [ ! -f "Cargo.toml" ] || [ ! -f "src/main.rs" ]; then
    echo "Error: This script must be run from the tuimu directory"
    exit 1
fi

# Ask for confirmation
echo "This will clean up all tuimu and Rust caches. Continue? (y/N)"
read -r response
if [[ ! "$response" =~ ^[Yy]$ ]]; then
    echo "Cancelled."
    exit 0
fi

echo "Cleaning up caches..."

# Clean Rust build cache
echo "Cleaning Rust build cache..."
cargo clean

# Clear Cargo registry and git cache
echo "Clearing Cargo registry and git cache..."
rm -rf ~/.cargo/registry/cache/ 2>/dev/null || true
rm -rf ~/.cargo/git/db/ 2>/dev/null || true

# Clear yt-dlp cache
echo "Clearing yt-dlp cache..."
rm -rf ~/.cache/yt-dlp/ 2>/dev/null || true

# Clear mpv cache
echo "Clearing mpv cache..."
rm -rf ~/.mpv/ 2>/dev/null || true

# Clear application cache
echo "Clearing tuimu application cache..."
rm -rf ~/.local/share/tuimu/ 2>/dev/null || true

echo "=== Cleanup complete ==="
echo "All caches have been cleared."
echo ""
echo "You can now build tuimu with:"
echo "  cargo build --release"
echo ""
echo "Or run the tests with:"
echo "  cargo test"