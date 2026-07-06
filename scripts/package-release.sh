#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
VERSION="${HOT_RUST_VERSION:-}"
TARGET="${HOT_RUST_TARGET:-}"

host_target_triple() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os:$arch" in
        Darwin:arm64) echo "aarch64-apple-darwin" ;;
        Darwin:x86_64) echo "x86_64-apple-darwin" ;;
        Linux:x86_64) echo "x86_64-unknown-linux-gnu" ;;
        Linux:aarch64) echo "aarch64-unknown-linux-gnu" ;;
        *)
            echo "package-release.sh: unsupported platform: $os $arch" >&2
            exit 1
            ;;
    esac
}

target_is_windows() {
    case "$1" in
        *windows*) return 0 ;;
        *) return 1 ;;
    esac
}

exe_name() {
    if target_is_windows "$1"; then
        echo "hr.exe"
    else
        echo "hr"
    fi
}

runtime_name() {
    case "$1" in
        *apple-darwin*) echo "libhr_runtime.dylib" ;;
        *unknown-linux-gnu*) echo "libhr_runtime.so" ;;
        *windows*) echo "" ;;
        *)
            echo "package-release.sh: unsupported runtime target: $1" >&2
            exit 1
            ;;
    esac
}

[ -n "$TARGET" ] || TARGET="$(host_target_triple)"
EXE="$(exe_name "$TARGET")"
RUNTIME="$(runtime_name "$TARGET")"
PACKAGE="hot-rust-$TARGET"
DIST="$ROOT/dist"
STAGE="$DIST/$PACKAGE"
BUILD_DIR="$ROOT/target/$TARGET/release"

cd "$ROOT"
if target_is_windows "$TARGET"; then
    cargo build --release --target "$TARGET" --bin hr
else
    cargo build --release --target "$TARGET" --bin hr --lib
fi

rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$BUILD_DIR/$EXE" "$STAGE/$EXE"
if [ -n "$RUNTIME" ]; then
    cp "$BUILD_DIR/$RUNTIME" "$STAGE/$RUNTIME"
fi
cp "$ROOT/install.sh" "$STAGE/install.sh"
chmod 755 "$STAGE/$EXE" "$STAGE/install.sh"
if [ -n "$RUNTIME" ]; then
    chmod 644 "$STAGE/$RUNTIME"
fi

cat > "$STAGE/README.txt" <<EOF
hot-rust $TARGET${VERSION:+ $VERSION}

Install:
  ./install.sh --from .

Use:
  cd your-rust-project
  hr cargo run
EOF
if target_is_windows "$TARGET"; then
    cat >> "$STAGE/README.txt" <<'EOF'

Windows package note:
  This package installs hr.exe as a Cargo wrapper. Runtime live patching is not
  shipped for Windows yet.
EOF
fi

(
    cd "$DIST"
    tar -czf "$PACKAGE.tar.gz" "$PACKAGE"
    shasum -a 256 "$PACKAGE.tar.gz" > "$PACKAGE.tar.gz.sha256"
)

echo "release assets:"
echo "  $DIST/$PACKAGE.tar.gz"
echo "  $DIST/$PACKAGE.tar.gz.sha256"
echo "  $ROOT/install.sh"
