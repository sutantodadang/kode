#!/bin/sh
# Kode installer for Linux/macOS.
# Usage: curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/install.sh | sh
set -eu

REPO="sutantodadang/kode"
BIN_NAME="kode"

log() {
    echo "kode: $1"
}

err() {
    echo "kode: error: $1" >&2
    exit 1
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)
            os_part="unknown-linux-gnu"
            ;;
        Darwin)
            os_part="apple-darwin"
            ;;
        *)
            err "unsupported OS '$os'. Windows users should use install.ps1 instead."
            ;;
    esac

    case "$arch" in
        x86_64 | amd64)
            arch_part="x86_64"
            ;;
        aarch64 | arm64)
            arch_part="aarch64"
            ;;
        *)
            err "unsupported architecture '$arch'."
            ;;
    esac

    TARGET="${arch_part}-${os_part}"

    case "$TARGET" in
        x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-apple-darwin | aarch64-apple-darwin)
            : # supported
            ;;
        *)
            err "unsupported OS/architecture combination '$TARGET'."
            ;;
    esac
}

detect_downloader() {
    if command -v curl >/dev/null 2>&1; then
        DOWNLOADER="curl"
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOADER="wget"
    else
        err "neither curl nor wget is available. Please install one and retry."
    fi
}

download() {
    # download <url> <output_path>
    url="$1"
    out="$2"
    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsSL -o "$out" "$url"
    else
        wget -q -O "$out" "$url"
    fi
}

detect_target
detect_downloader

if [ "${KODE_VERSION:-}" != "" ]; then
    BASE_URL="https://github.com/${REPO}/releases/download/${KODE_VERSION}/kode-${TARGET}.tar.gz"
else
    BASE_URL="https://github.com/${REPO}/releases/latest/download/kode-${TARGET}.tar.gz"
fi
SHA_URL="${BASE_URL}.sha256"

TMP_DIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ARCHIVE="$TMP_DIR/kode-${TARGET}.tar.gz"
ARCHIVE_SHA="$TMP_DIR/kode-${TARGET}.tar.gz.sha256"

log "downloading kode for ${TARGET}..."
download "$BASE_URL" "$ARCHIVE"

log "downloading checksum..."
if ! download "$SHA_URL" "$ARCHIVE_SHA" 2>/dev/null; then
    log "warning: could not download checksum file, skipping verification."
    ARCHIVE_SHA=""
fi

if [ -n "$ARCHIVE_SHA" ] && [ -f "$ARCHIVE_SHA" ]; then
    (
        cd "$TMP_DIR"
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum -c "$(basename "$ARCHIVE_SHA")"
        elif command -v shasum >/dev/null 2>&1; then
            shasum -a 256 -c "$(basename "$ARCHIVE_SHA")"
        else
            log "warning: no sha256sum or shasum found, skipping checksum verification."
            exit 0
        fi
    ) || err "checksum verification failed."
else
    log "warning: skipping checksum verification (no checksum file)."
fi

log "extracting archive..."
tar -xzf "$ARCHIVE" -C "$TMP_DIR"

INSTALL_DIR="${KODE_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"

if [ ! -f "$TMP_DIR/$BIN_NAME" ]; then
    err "extracted archive does not contain expected binary '$BIN_NAME'."
fi

install -m 755 "$TMP_DIR/$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"

log "installed $BIN_NAME to $INSTALL_DIR/$BIN_NAME"

case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        ;;
    *)
        log "note: $INSTALL_DIR is not in your PATH."
        log "add this line to your shell profile (e.g. ~/.bashrc, ~/.zshrc):"
        log "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

"$INSTALL_DIR/$BIN_NAME" --version

log "kode installed successfully. Run 'kode doctor' to verify your setup."
