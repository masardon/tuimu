#!/usr/bin/env bash

# TuiMu Installation Script
# - Linux: installs a pre-built release binary without requiring Cargo.
# - Termux (Android): installs build/runtime dependencies via pkg and
#   builds from source (there is no pre-built Android release).

set -e

echo "=== TuiMu Installation Script ==="

# --- Platform detection ------------------------------------------------------

if [ -n "${TERMUX_VERSION:-}" ] || [ "${PREFIX:-}" = "/data/data/com.termux/files/usr" ]; then
    IS_TERMUX=1
else
    IS_TERMUX=0
fi

# Scratch space shared by both branches, cleaned up on exit.
TEMP_DIR=""
CLONE_DIR=""
cleanup() {
    [ -n "$TEMP_DIR" ] && rm -rf "$TEMP_DIR"
    [ -n "$CLONE_DIR" ] && rm -rf "$CLONE_DIR"
    return 0
}
trap cleanup EXIT

# --- Linux: pre-built binary --------------------------------------------------

install_linux() {
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
    case "$ARCH" in
        x86_64 | aarch64 | armv7l) ;;
        *)
            echo "Error: Unsupported architecture: $ARCH"
            exit 1
            ;;
    esac

    DOWNLOAD_URL="https://github.com/masardon/tuimu/releases/latest/download/tuimu-linux-$ARCH.tar.gz"

    TEMP_DIR=$(mktemp -d)
    cd "$TEMP_DIR"

    echo "Downloading TuiMu for $ARCH..."
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
}

# --- Termux: dependencies via pkg, pre-built release or source build ------------

install_termux() {
    echo "Detected Termux (Android)."

    # Runtime dependencies (needed for every install path).
    command -v curl >/dev/null || pkg install -y curl
    command -v mpv >/dev/null || pkg install -y mpv
    # Only needed for the visualizer; missing pulseaudio falls back to
    # Android's native audio output, so this is best-effort.
    command -v pulseaudio >/dev/null || pkg install -y pulseaudio || \
        echo "Warning: pulseaudio not installed; the visualizer will be inactive."
    if ! command -v yt-dlp >/dev/null; then
        # The repository package can lag behind YouTube API changes; fall
        # back to pip, which always installs the latest release.
        pkg install -y yt-dlp || { pkg install -y python-pip && pip install yt-dlp; }
    fi

    # Prefer a pre-built release when one exists for this architecture.
    ARCH=$(uname -m)
    if [ "$ARCH" = "aarch64" ] && install_android_prebuilt "$ARCH"; then
        return 0
    fi
    install_from_source
}

# Downloads and installs a pre-built Android binary. Returns non-zero when
# no matching release exists (older releases, forks, network problems), in
# which case the caller falls back to building from source.
install_android_prebuilt() {
    ARCH=$1
    URL="https://github.com/masardon/tuimu/releases/latest/download/tuimu-android-$ARCH.tar.gz"

    TEMP_DIR=$(mktemp -d)
    echo "Downloading TuiMu for Android $ARCH..."
    if ! curl -fL -o "$TEMP_DIR/tuimu.tar.gz" "$URL"; then
        echo "No pre-built Android build available; building from source instead."
        rm -rf "$TEMP_DIR"; TEMP_DIR=""
        return 1
    fi
    tar -xzf "$TEMP_DIR/tuimu.tar.gz" -C "$TEMP_DIR" || return 1
    BINARY=$(find "$TEMP_DIR" -type f -name tuimu -print -quit)
    if [ -z "$BINARY" ]; then
        echo "Error: Could not find tuimu binary in the archive"
        return 1
    fi

    echo "Installing to \$PREFIX/bin..."
    install -m 0755 "$BINARY" "$PREFIX/bin/tuimu"
}

install_from_source() {
    # Build toolchain.
    command -v cargo >/dev/null || pkg install -y rust binutils
    command -v git >/dev/null || pkg install -y git

    # Prefer a checkout this script lives in (developer/CI use); otherwise
    # clone the repository.
    SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
    if [ -f "$SCRIPT_DIR/Cargo.toml" ] && [ -d "$SCRIPT_DIR/.git" ]; then
        SRC_DIR="$SCRIPT_DIR"
        echo "Building from checkout: $SRC_DIR"
    else
        TEMP_DIR=$(mktemp -d)
        echo "Cloning TuiMu..."
        git clone --depth 1 https://github.com/masardon/tuimu.git "$TEMP_DIR/tuimu"
        SRC_DIR="$TEMP_DIR/tuimu"
    fi

    echo "Building (this takes a few minutes)..."
    (cd "$SRC_DIR" && cargo build --release)

    echo "Installing to \$PREFIX/bin..."
    install -m 0755 "$SRC_DIR/target/release/tuimu" "$PREFIX/bin/tuimu"
}

# --- Run ------------------------------------------------------------------------

if [ "$IS_TERMUX" -eq 1 ]; then
    install_termux
else
    install_linux
fi

echo "=== Installation complete ==="
if [ "$IS_TERMUX" -eq 1 ]; then
    echo "TuiMu has been installed to \$PREFIX/bin/tuimu"
else
    echo "TuiMu has been installed to /usr/local/bin/tuimu"
fi
echo ""

# Another tuimu earlier in PATH would silently shadow this installation.
FIRST_TUIMU=$(command -v tuimu || true)
if [ -n "$FIRST_TUIMU" ] && ! [ "$FIRST_TUIMU" -ef "$PREFIX/bin/tuimu" ] && ! [ "$FIRST_TUIMU" -ef "/usr/local/bin/tuimu" ]; then
    echo "Warning: 'tuimu' currently resolves to $FIRST_TUIMU, which shadows this install."
    echo "Remove it or reorder PATH so the new binary is used."
    echo ""
fi

MISSING=0
for dep in yt-dlp mpv curl; do
    if ! command -v "$dep" &> /dev/null; then
        echo "Warning: '$dep' was not found in PATH; TuiMu needs it at runtime."
        MISSING=1
    fi
done
if [ "$IS_TERMUX" -eq 1 ] && ! command -v pulseaudio &> /dev/null; then
    echo "Note: 'pulseaudio' was not found; it is only needed for the visualizer."
fi
if [ "$MISSING" -eq 1 ]; then
    echo ""
    echo "Install missing dependencies, then run 'tuimu'."
else
    echo "Run it with: tuimu"
fi
