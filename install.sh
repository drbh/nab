#!/bin/sh
set -e

# nab installer
# Usage: curl -LsSf nab.dholtz.com/install.sh | sh

REPO="drbh/nab"
INSTALL_DIR="${HOME}/.local/bin"

main() {
    platform=$(detect_platform)

    if [ -z "$platform" ]; then
        err "Unsupported platform: $(uname -s)/$(uname -m)"
    fi

    echo "Detected platform: $platform"

    # Get latest release
    release_url="https://api.github.com/repos/${REPO}/releases/latest"
    release_info=$(curl -sS "$release_url") || err "Failed to fetch release info"

    version=$(echo "$release_info" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')

    if [ -z "$version" ]; then
        err "Could not determine latest version"
    fi

    echo "Latest version: $version"

    # Find matching asset
    download_url=$(echo "$release_info" | grep "browser_download_url" | grep "$platform" | head -1 | sed 's/.*"\(https[^"]*\)".*/\1/')

    if [ -z "$download_url" ]; then
        err "No asset found for platform: $platform"
    fi

    echo "Downloading from: $download_url"

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    # Download and install
    tmp=$(mktemp)
    curl -LSsf "$download_url" -o "$tmp" || err "Download failed"

    # Check if it's an archive or raw binary
    case "$download_url" in
        *.tar.gz|*.tgz)
            tar -xzf "$tmp" -C "$INSTALL_DIR" --strip-components=1 nab 2>/dev/null || \
            tar -xzf "$tmp" -O > "$INSTALL_DIR/nab"
            ;;
        *.zip)
            unzip -p "$tmp" > "$INSTALL_DIR/nab"
            ;;
        *)
            mv "$tmp" "$INSTALL_DIR/nab"
            ;;
    esac

    chmod +x "$INSTALL_DIR/nab"
    rm -f "$tmp"

    echo ""
    echo "Installed nab to $INSTALL_DIR/nab"
    echo ""

    # Check if in PATH
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) ;;
        *)
            echo "Add to your PATH:"
            echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
            echo ""
            ;;
    esac

    echo "Run 'nab --help' to get started"
}

detect_platform() {
    os=$(uname -s | tr '[:upper:]' '[:lower:]')
    arch=$(uname -m)

    case "$os" in
        darwin)
            case "$arch" in
                arm64|aarch64) echo "darwin-arm64" ;;
                x86_64|amd64) echo "darwin-amd64" ;;
            esac
            ;;
        linux)
            case "$arch" in
                x86_64|amd64) echo "linux-amd64" ;;
                aarch64|arm64) echo "linux-arm64" ;;
                riscv64) echo "linux-riscv64" ;;
            esac
            ;;
    esac
}

err() {
    echo "Error: $1" >&2
    exit 1
}

main
