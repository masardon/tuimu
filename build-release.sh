#!/bin/bash

# Tuimu Build Script for Release
# This script builds tuimu for release and packages it

set -e

echo "=== Tuimu Build Script ==="

# Get version from Cargo.toml
VERSION=$(grep "^version" Cargo.toml | cut -d '"' -f 2)
echo "Building version: $VERSION"

# Clean previous builds
echo "Cleaning previous builds..."
cargo clean

# Run tests
echo "Running tests..."
cargo test

# Build for release
echo "Building for release..."
cargo build --release

# Create release notes
cat > release_notes.md << EOF
tuimu v$VERSION

This release includes:
- Fixed F1, F2, F3 key functionality
- Added lyrics toggle with F3
- Improved search and navigation
- Enhanced UI with better highlighting
- Fixed cache management
- Added installation scripts

Changes:
- F1 now toggles help window
- F2 cycles through search sources
- F3 toggles lyrics window
- Fixed cursor movement in results list
- Improved search input handling
- Added comprehensive documentation
- Added cache management scripts
- Added installation scripts for Linux

Installation:
- cargo install --git https://github.com/masardon/tuimu
- Or use the provided install.sh script

For more information, see the README.md file.
EOF

# Create package directory
PACKAGE_DIR="tuimu-linux-x86_64"
mkdir -p "$PACKAGE_DIR"

# Copy binaries and scripts
cp target/release/tuimu "$PACKAGE_DIR/"
cp README.md "$PACKAGE_DIR/"
cp clean-cache.sh "$PACKAGE_DIR/"
cp install.sh "$PACKAGE_DIR/"

# Create archive
echo "Creating archive..."
tar -czf "tuimu-linux-x86_64.tar.gz" "$PACKAGE_DIR/"

# Clean up
rm -rf "$PACKAGE_DIR"

echo "=== Build complete ==="
echo "Release package created: tuimu-linux-x86_64.tar.gz"
echo ""
echo "To install, run:"
echo "  ./install.sh"
echo ""
echo "To clean caches before build, run:"
echo "  ./clean-cache.sh"
echo ""
echo "To submit to GitHub releases:"
echo "  gh release create v$VERSION tuimu-linux-x86_64.tar.gz --title \"tuimu v$VERSION\" --notes \"$(cat release_notes.md)\""