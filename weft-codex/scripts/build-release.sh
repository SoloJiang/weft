#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
VERSION=${WEFT_CODEX_VERSION:-0.1.1}
BUILD_NUMBER=$(date -u +%Y%m%d%H%M%S)
MACHINE_ARCH=$(uname -m)

case "$MACHINE_ARCH" in
  arm64) PACKAGE_ARCH=arm64 ;;
  x86_64) PACKAGE_ARCH=x86_64 ;;
  *)
    echo "Unsupported macOS architecture: $MACHINE_ARCH" >&2
    exit 1
    ;;
esac

if [ "$(uname -s)" != "Darwin" ]; then
  echo "The Desktop Host release currently supports macOS only." >&2
  exit 1
fi

for command_name in cargo pnpm bun tar shasum; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "Missing release dependency: $command_name" >&2
    exit 1
  fi
done

OUTPUT_INPUT=${1:-"$PROJECT_ROOT/artifacts"}
mkdir -p "$OUTPUT_INPUT"
OUTPUT_DIR=$(CDPATH= cd -- "$OUTPUT_INPUT" && pwd)
STAGING_DIR=$(mktemp -d /private/tmp/weft-codex-release.XXXXXX)
trap 'rm -rf "$STAGING_DIR"' EXIT HUP INT TERM

echo "[release] verifying UI"
pnpm --dir "$PROJECT_ROOT/ui" install --frozen-lockfile
pnpm --dir "$PROJECT_ROOT/ui" typecheck
pnpm --dir "$PROJECT_ROOT/ui" build

echo "[release] verifying Desktop Host"
pnpm --dir "$PROJECT_ROOT/launcher" install --frozen-lockfile
pnpm --dir "$PROJECT_ROOT/launcher" typecheck
pnpm --dir "$PROJECT_ROOT/launcher" test

echo "[release] verifying and building daemon"
cargo test --manifest-path "$PROJECT_ROOT/Cargo.toml" --workspace --locked
cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml" --release --locked -p weftd

echo "[release] compiling self-contained Host"
bun build "$PROJECT_ROOT/launcher/src/cli.ts" \
  --compile \
  --outfile "$STAGING_DIR/weft-codex"
"$STAGING_DIR/weft-codex" --version >/dev/null

RUNTIME_NAME="weft-codex-$VERSION-macos-$PACKAGE_ARCH"
RUNTIME_ROOT="$STAGING_DIR/$RUNTIME_NAME"
mkdir -p "$RUNTIME_ROOT/bin" "$RUNTIME_ROOT/libexec" \
  "$RUNTIME_ROOT/share/weft-codex/web" \
  "$RUNTIME_ROOT/share/weft-codex/skills"
install -m 755 "$STAGING_DIR/weft-codex" "$RUNTIME_ROOT/bin/weft-codex"
install -m 755 "$PROJECT_ROOT/target/release/weftd" "$RUNTIME_ROOT/bin/weftd"
install -m 755 "$PROJECT_ROOT/packaging/install.sh" "$RUNTIME_ROOT/install.sh"
install -m 755 "$PROJECT_ROOT/packaging/weft-codex-wrapper" \
  "$RUNTIME_ROOT/libexec/weft-codex-wrapper"
cp -R "$PROJECT_ROOT/crates/daemon/web/." "$RUNTIME_ROOT/share/weft-codex/web/"
if [ ! -d "$PROJECT_ROOT/skills" ]; then
  echo "Missing product skills directory: $PROJECT_ROOT/skills" >&2
  exit 1
fi
cp -R "$PROJECT_ROOT/skills/." "$RUNTIME_ROOT/share/weft-codex/skills/"
install -m 644 "$PROJECT_ROOT/packaging/RELEASE-README.md" "$RUNTIME_ROOT/README.md"
printf '{\n  "version": "%s",\n  "build": "%s",\n  "platform": "macos",\n  "architecture": "%s"\n}\n' \
  "$VERSION" "$BUILD_NUMBER" "$PACKAGE_ARCH" > "$RUNTIME_ROOT/manifest.json"

if command -v codesign >/dev/null 2>&1; then
  codesign --force --sign - "$RUNTIME_ROOT/bin/weft-codex" >/dev/null
  codesign --force --sign - "$RUNTIME_ROOT/bin/weftd" >/dev/null
fi

echo "[release] verifying assembled Host"
"$RUNTIME_ROOT/bin/weft-codex" doctor >/dev/null

ARCHIVE_NAME="$RUNTIME_NAME.tar.gz"
COPYFILE_DISABLE=1 tar -C "$STAGING_DIR" -czf "$OUTPUT_DIR/$ARCHIVE_NAME" "$RUNTIME_NAME"

(
  cd "$OUTPUT_DIR"
  shasum -a 256 "$ARCHIVE_NAME" > "$ARCHIVE_NAME.sha256"
)

echo "[release] wrote:"
echo "  $OUTPUT_DIR/$ARCHIVE_NAME"
