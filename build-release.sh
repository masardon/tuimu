#!/bin/bash

# Build tuimu for release and package the binary with documentation.

set -e

echo "=== Tuimu Build Script ==="

VERSION=$(grep "^version" Cargo.toml | cut -d '"' -f 2)
echo "Building version: $VERSION"

ARCH=$(uname -m)
case $ARCH in
    x86_64 | aarch64 | armv7l) ;;
    *)
        echo "Error: Unsupported architecture: $ARCH"
        exit 1
        ;;
esac
echo "Target architecture: $ARCH"

echo "Running tests..."
cargo test

echo "Building for release..."
cargo build --release

cat > release_notes.md << EOF
tuimu v$VERSION

A fast, free terminal music player.

- Search YouTube and SoundCloud with yt-dlp
- Paste any URL supported by yt-dlp
- Queue management, repeat modes, seek and volume controls
- Synced and plain lyrics from LrcLib
- Quality filters and sorting

Install:
- cargo install --git https://github.com/masardon/tuimu
- Or download tuimu-linux-$ARCH.tar.gz and run ./install.sh

See README.md for key bindings and usage.
EOF

PACKAGE_DIR="tuimu-linux-$ARCH"
ARCHIVE="$PACKAGE_DIR.tar.gz"
rm -rf "$PACKAGE_DIR" "$ARCHIVE"
mkdir -p "$PACKAGE_DIR"

cp target/release/tuimu "$PACKAGE_DIR/"
cp README.md LICENSE clean-cache.sh install.sh "$PACKAGE_DIR/"

echo "Creating archive..."
tar -czf "$ARCHIVE" "$PACKAGE_DIR/"
rm -rf "$PACKAGE_DIR"

echo "=== Build complete ==="
echo "Release package created: $ARCHIVE"
echo ""
echo "To submit to GitHub releases:"
echo "  gh release create v$VERSION \"$ARCHIVE\" --title \"tuimu v$VERSION\" --notes-file release_notes.md"
