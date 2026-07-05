#!/bin/sh
set -eu

ROOT="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
VERSION="${HOT_RUST_VERSION:-}"
TARGET="${HOT_RUST_TARGET:-}"

target_triple() {
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

runtime_name() {
    case "$(uname -s)" in
        Darwin) echo "libhr_runtime.dylib" ;;
        Linux) echo "libhr_runtime.so" ;;
        *)
            echo "package-release.sh: unsupported runtime platform: $(uname -s)" >&2
            exit 1
            ;;
    esac
}

[ -n "$TARGET" ] || TARGET="$(target_triple)"
RUNTIME="$(runtime_name)"
PACKAGE="hot-rust-$TARGET"
DIST="$ROOT/dist"
STAGE="$DIST/$PACKAGE"

cd "$ROOT"
cargo build --release --bin hr --lib

rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$ROOT/target/release/hr" "$STAGE/hr"
cp "$ROOT/target/release/$RUNTIME" "$STAGE/$RUNTIME"
cp "$ROOT/install.sh" "$STAGE/install.sh"
chmod 755 "$STAGE/hr" "$STAGE/install.sh"
chmod 644 "$STAGE/$RUNTIME"

cat > "$STAGE/README.txt" <<EOF
hot-rust $TARGET${VERSION:+ $VERSION}

Install:
  ./install.sh --from .

Use:
  cd your-rust-project
  hr cargo run
EOF

(
    cd "$DIST"
    tar -czf "$PACKAGE.tar.gz" "$PACKAGE"
    shasum -a 256 "$PACKAGE.tar.gz" > "$PACKAGE.tar.gz.sha256"
)

echo "release assets:"
echo "  $DIST/$PACKAGE.tar.gz"
echo "  $DIST/$PACKAGE.tar.gz.sha256"
echo "  $ROOT/install.sh"
