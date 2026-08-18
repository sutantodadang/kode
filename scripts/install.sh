#!/bin/sh
# Kode installer for Linux and macOS.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/sutantodadang/kode/main/scripts/install.sh | sh -s -- v0.1.0
#
# Env overrides:
#   KODE_INSTALL_DIR   install directory (default: $HOME/.kode/bin)
set -eu

REPO="sutantodadang/kode"
INSTALL_DIR="${KODE_INSTALL_DIR:-$HOME/.kode/bin}"
VERSION="${1:-}"

err() {
    echo "kode-install: error: $1" >&2
    exit 1
}

info() {
    echo "kode-install: $1"
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1
}

# ---- detect OS ----
os_raw="$(uname -s)"
case "$os_raw" in
    Linux) os="linux" ;;
    Darwin) os="darwin" ;;
    *) err "unsupported OS: $os_raw (Kode ships prebuilt binaries for Linux and macOS only)" ;;
esac

# ---- detect arch ----
arch_raw="$(uname -m)"
case "$arch_raw" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) err "unsupported architecture: $arch_raw (Kode ships x86_64 and aarch64 binaries only)" ;;
esac

# ---- resolve target triple ----
if [ "$os" = "linux" ]; then
    target="${arch}-unknown-linux-gnu"
else
    target="${arch}-apple-darwin"
fi

asset="kode-${target}.tar.gz"

# ---- pick downloader ----
if need_cmd curl; then
    downloader="curl"
elif need_cmd wget; then
    downloader="wget"
else
    err "neither curl nor wget found — install one and re-run"
fi

fetch_stdout() {
    # fetch_stdout <url>
    if [ "$downloader" = "curl" ]; then
        curl -fsSL "$1"
    else
        wget -qO- "$1"
    fi
}

fetch_to_file() {
    # fetch_to_file <url> <dest>
    if [ "$downloader" = "curl" ]; then
        curl -fsSL -o "$2" "$1"
    else
        wget -qO "$2" "$1"
    fi
}

resolve_redirect_tag() {
    # Fallback: ask for the redirect target of /releases/latest and read the
    # tag out of the Location header, without requiring the GitHub API.
    if [ "$downloader" = "curl" ]; then
        location="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)"
    else
        location="$(wget -q --max-redirect=20 --spider -S "https://github.com/${REPO}/releases/latest" 2>&1 | awk '/Location: /{loc=$2} END{print loc}')"
    fi
    printf '%s\n' "$location" | sed -n 's#.*/releases/tag/\(v[^/[:space:]]*\).*#\1#p' | tail -n1
}

# ---- resolve version/tag ----
if [ -n "$VERSION" ]; then
    case "$VERSION" in
        v*) tag="$VERSION" ;;
        *) tag="v$VERSION" ;;
    esac
    info "using requested version $tag"
else
    info "resolving latest release..."
    api_json="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null || true)"
    tag="$(printf '%s' "$api_json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"

    if [ -z "$tag" ]; then
        info "GitHub API lookup failed, falling back to redirect resolution..."
        tag="$(resolve_redirect_tag)"
    fi

    [ -n "$tag" ] || err "could not resolve the latest release tag — pass a version explicitly, e.g. 'sh install.sh v0.1.0'"
fi

info "installing kode ${tag} for ${target}..."

# ---- download ----
tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t kode-install)"
trap 'rm -rf "$tmp_dir"' EXIT INT TERM

asset_url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
sha_url="${asset_url}.sha256"
archive_path="${tmp_dir}/${asset}"
sha_path="${archive_path}.sha256"

fetch_to_file "$asset_url" "$archive_path" || err "failed to download ${asset_url}
Check that a release exists for ${tag} and target ${target} at:
  https://github.com/${REPO}/releases"

# ---- verify checksum (best effort — skip if sidecar missing) ----
if fetch_to_file "$sha_url" "$sha_path" 2>/dev/null; then
    expected="$(awk '{print $1}' "$sha_path" | head -n1)"
    if need_cmd sha256sum; then
        actual="$(sha256sum "$archive_path" | awk '{print $1}')"
    elif need_cmd shasum; then
        actual="$(shasum -a 256 "$archive_path" | awk '{print $1}')"
    else
        actual=""
    fi

    if [ -n "$actual" ]; then
        expected_lc="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
        actual_lc="$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')"
        [ "$expected_lc" = "$actual_lc" ] || err "checksum mismatch for ${asset} — aborting install"
        info "checksum verified"
    else
        info "no sha256sum/shasum found — skipping checksum verification"
    fi
else
    info "no checksum sidecar found for ${asset} — skipping checksum verification"
fi

# ---- extract ----
tar -xzf "$archive_path" -C "$tmp_dir" || err "failed to extract ${archive_path}"
[ -f "${tmp_dir}/kode" ] || err "extracted archive did not contain a 'kode' binary"

# ---- install ----
mkdir -p "$INSTALL_DIR" || err "failed to create install directory: $INSTALL_DIR"
cp "${tmp_dir}/kode" "${INSTALL_DIR}/kode" || err "failed to copy binary to $INSTALL_DIR"
chmod +x "${INSTALL_DIR}/kode"

info "installed kode ${tag} to ${INSTALL_DIR}/kode"

# ---- PATH check ----
case ":$PATH:" in
    *":$INSTALL_DIR:"*)
        info "${INSTALL_DIR} is already on your PATH"
        ;;
    *)
        rc_file=""
        case "${SHELL:-}" in
            */zsh) rc_file="$HOME/.zshrc" ;;
            */bash) rc_file="$HOME/.bashrc" ;;
            *) rc_file="$HOME/.profile" ;;
        esac
        echo ""
        echo "kode-install: ${INSTALL_DIR} is not on your PATH."
        echo "kode-install: add this line to ${rc_file} (or your shell's profile), then open a new shell:"
        echo ""
        echo "    export PATH=\"${INSTALL_DIR}:\$PATH\""
        echo ""
        ;;
esac

info "run 'kode --version' to verify, then 'kode doctor' for a full diagnostic."
