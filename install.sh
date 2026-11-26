#!/usr/bin/env sh
set -eu

REPO="Cukkoo12/ctxproof"
INSTALL_DIR="${CTXPROOF_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${CTXPROOF_VERSION:-}"

need_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "ctxproof installer error: required command '$1' was not found" >&2
        exit 1
    fi
}

need_cmd curl
need_cmd tar

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS:$ARCH" in
    Linux:x86_64|Linux:amd64)
        TARGET="x86_64-unknown-linux-gnu"
        ;;
    Darwin:arm64|Darwin:aarch64)
        TARGET="aarch64-apple-darwin"
        ;;
    Darwin:x86_64)
        echo "Intel macOS prebuilt binary is not published yet." >&2
        echo "Install from source with Rust: cargo install --git https://github.com/$REPO --locked" >&2
        exit 1
        ;;
    *)
        echo "Unsupported platform '$OS/$ARCH' for prebuilt archive." >&2
        echo "Install from source with Rust: cargo install --git https://github.com/$REPO --locked" >&2
        exit 1
        ;;
esac

if [ -z "$VERSION" ]; then
    EFFECTIVE_URL=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")
    VERSION="${EFFECTIVE_URL##*/}"
    case "$VERSION" in
        v*) ;;
        *)
            echo "Could not resolve latest release tag from GitHub (got '$VERSION')." >&2
            exit 1
            ;;
    esac
fi

ARCHIVE="ctxproof-${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/$REPO/releases/download/${VERSION}"
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

echo "Downloading ctxproof ${VERSION} (${TARGET})..."
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMP_DIR}/${ARCHIVE}"
curl -fsSL "${BASE_URL}/${ARCHIVE}.sha256" -o "${TMP_DIR}/${ARCHIVE}.sha256"

EXPECTED_SHA=$(awk '{print $1}' "${TMP_DIR}/${ARCHIVE}.sha256" | tr '[:upper:]' '[:lower:]')
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA=$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_SHA=$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}' | tr '[:upper:]' '[:lower:]')
else
    echo "ctxproof installer error: neither sha256sum nor shasum is installed" >&2
    exit 1
fi

if [ "$EXPECTED_SHA" != "$ACTUAL_SHA" ]; then
    echo "SHA256 verification failed for ${ARCHIVE} (expected ${EXPECTED_SHA}, got ${ACTUAL_SHA})" >&2
    exit 1
fi

tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "${TMP_DIR}"
mkdir -p "$INSTALL_DIR"
install -m 755 "${TMP_DIR}/ctxproof" "${INSTALL_DIR}/ctxproof"

echo "Installed $("${INSTALL_DIR}/ctxproof" --version) to ${INSTALL_DIR}/ctxproof"
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        echo "Add ${INSTALL_DIR} to your PATH:"
        echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
        ;;
esac
echo "Run 'ctxproof init' inside any repository to configure .ctxproof.toml and MCP rules."
