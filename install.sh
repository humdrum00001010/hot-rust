#!/bin/sh
set -eu

REPO="${HOT_RUST_REPO:-humdrum00001010/hot-rust}"
VERSION="${HOT_RUST_VERSION:-latest}"
INSTALL_DIR="${HOT_RUST_INSTALL_DIR:-$HOME/.local/lib/hot-rust}"
BIN_DIR="${HOT_RUST_BIN_DIR:-$HOME/.local/bin}"
FROM=""

usage() {
    cat <<'EOF'
usage: install.sh [options]

Options:
  --version <tag>      Install a specific GitHub release tag. Default: latest.
  --repo <owner/repo>  GitHub repository. Default: humdrum00001010/hot-rust.
  --install-dir <dir>  Install paired files here. Default: ~/.local/lib/hot-rust.
  --bin-dir <dir>      Write the hr command here. Default: ~/.local/bin.
  --from <path>        Install from a local release tarball or unpacked directory.
  -h, --help           Show this help.

Environment variables mirror the long options:
  HOT_RUST_VERSION, HOT_RUST_REPO, HOT_RUST_INSTALL_DIR, HOT_RUST_BIN_DIR
  HOT_RUST_NO_SHELL_RC=1 skips shell profile PATH setup.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "install.sh: --version requires a value" >&2; exit 2; }
            VERSION="$2"
            shift 2
            ;;
        --repo)
            [ "$#" -ge 2 ] || { echo "install.sh: --repo requires a value" >&2; exit 2; }
            REPO="$2"
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { echo "install.sh: --install-dir requires a value" >&2; exit 2; }
            INSTALL_DIR="$2"
            shift 2
            ;;
        --bin-dir)
            [ "$#" -ge 2 ] || { echo "install.sh: --bin-dir requires a value" >&2; exit 2; }
            BIN_DIR="$2"
            shift 2
            ;;
        --from)
            [ "$#" -ge 2 ] || { echo "install.sh: --from requires a value" >&2; exit 2; }
            FROM="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "install.sh: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "install.sh: missing required command: $1" >&2
        exit 1
    }
}

target_triple() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os:$arch" in
        Darwin:arm64) echo "aarch64-apple-darwin" ;;
        Darwin:x86_64) echo "x86_64-apple-darwin" ;;
        Linux:x86_64) echo "x86_64-unknown-linux-gnu" ;;
        Linux:aarch64) echo "aarch64-unknown-linux-gnu" ;;
        *)
            echo "install.sh: unsupported platform: $os $arch" >&2
            exit 1
            ;;
    esac
}

runtime_name() {
    case "$(uname -s)" in
        Darwin) echo "libhr_runtime.dylib" ;;
        Linux) echo "libhr_runtime.so" ;;
        *)
            echo "install.sh: unsupported runtime platform: $(uname -s)" >&2
            exit 1
            ;;
    esac
}

download_url() {
    asset="$1"
    if [ "$VERSION" = "latest" ]; then
        echo "https://github.com/$REPO/releases/latest/download/$asset"
    else
        echo "https://github.com/$REPO/releases/download/$VERSION/$asset"
    fi
}

unpack_source() {
    dest="$1"
    if [ -n "$FROM" ]; then
        if [ -d "$FROM" ]; then
            echo "$FROM"
            return
        fi
        tar -xzf "$FROM" -C "$dest"
    else
        need_cmd curl
        asset="hot-rust-$(target_triple).tar.gz"
        url="$(download_url "$asset")"
        archive="$dest/$asset"
        echo "hot-rust: downloading $url"
        curl -fL --retry 3 --retry-delay 1 "$url" -o "$archive"
        tar -xzf "$archive" -C "$dest"
    fi

    found="$(find "$dest" -type f -name hr -perm -111 -print | head -n 1 || true)"
    [ -n "$found" ] || {
        echo "install.sh: release archive does not contain executable hr" >&2
        exit 1
    }
    dirname "$found"
}

shell_profile() {
    shell_name="$(basename "${SHELL:-}")"
    case "$shell_name" in
        zsh) echo "$HOME/.zshrc" ;;
        bash) echo "$HOME/.bashrc" ;;
        *) echo "$HOME/.profile" ;;
    esac
}

ensure_path() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) return ;;
    esac
    if [ "${HOT_RUST_NO_SHELL_RC:-0}" = "1" ]; then
        cat <<EOF
hot-rust: $BIN_DIR is not in PATH.
Run this or add it to your shell profile:
  export PATH="$BIN_DIR:\$PATH"
EOF
        return
    fi

    profile="$(shell_profile)"
    mkdir -p "$(dirname "$profile")"
    touch "$profile"
    if ! grep -F "$BIN_DIR" "$profile" >/dev/null 2>&1; then
        {
            echo ""
            echo "# hot-rust"
            echo "export PATH=\"$BIN_DIR:\$PATH\""
        } >> "$profile"
        echo "hot-rust: added $BIN_DIR to PATH in $profile"
    fi
}

need_cmd tar
need_cmd find

tmp="${TMPDIR:-/tmp}/hot-rust-install.$$"
rm -rf "$tmp"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT INT TERM

source_dir="$(unpack_source "$tmp")"
runtime="$(runtime_name)"

[ -x "$source_dir/hr" ] || {
    echo "install.sh: missing executable $source_dir/hr" >&2
    exit 1
}
[ -f "$source_dir/$runtime" ] || {
    echo "install.sh: missing runtime $source_dir/$runtime" >&2
    exit 1
}

mkdir -p "$INSTALL_DIR" "$BIN_DIR"
cp "$source_dir/hr" "$INSTALL_DIR/hr"
cp "$source_dir/$runtime" "$INSTALL_DIR/$runtime"
chmod 755 "$INSTALL_DIR/hr"
chmod 644 "$INSTALL_DIR/$runtime"

if command -v xattr >/dev/null 2>&1; then
    xattr -dr com.apple.quarantine "$INSTALL_DIR" >/dev/null 2>&1 || true
fi

cat > "$BIN_DIR/hr" <<EOF
#!/bin/sh
exec "$INSTALL_DIR/hr" "\$@"
EOF
chmod 755 "$BIN_DIR/hr"

ensure_path

cat <<EOF
hot-rust installed:
  command: $BIN_DIR/hr
  runtime: $INSTALL_DIR/$runtime

Use:
  cd your-rust-project
  hr cargo run
EOF
