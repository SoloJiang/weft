#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
VERSION=${WEFT_CODEX_VERSION:-0.1.1}
MACHINE_ARCH=$(uname -m)

case "$MACHINE_ARCH" in
  arm64) PACKAGE_ARCH=arm64 ;;
  x86_64) PACKAGE_ARCH=x86_64 ;;
  *)
    echo "Unsupported macOS architecture: $MACHINE_ARCH" >&2
    exit 1
    ;;
esac

ARCHIVE_INPUT=${1:-}
if [ -z "$ARCHIVE_INPUT" ]; then
  "$SCRIPT_DIR/build-release.sh"
  ARCHIVE_INPUT="$PROJECT_ROOT/artifacts/weft-codex-$VERSION-macos-$PACKAGE_ARCH.tar.gz"
fi

case "$ARCHIVE_INPUT" in
  /*) ARCHIVE=$ARCHIVE_INPUT ;;
  *) ARCHIVE=$(CDPATH= cd -- "$(dirname -- "$ARCHIVE_INPUT")" && pwd)/$(basename "$ARCHIVE_INPUT") ;;
esac

if [ ! -f "$ARCHIVE" ]; then
  echo "weft-codex install: archive not found: $ARCHIVE" >&2
  exit 1
fi

CHECKSUM_FILE="$ARCHIVE.sha256"
if [ -f "$CHECKSUM_FILE" ]; then
  (
    cd "$(dirname -- "$ARCHIVE")"
    shasum -a 256 -c "$(basename "$CHECKSUM_FILE")"
  )
fi

STAGING_DIR=$(mktemp -d /private/tmp/weft-codex-cli-install.XXXXXX)
trap 'rm -rf "$STAGING_DIR"' EXIT HUP INT TERM
tar -xzf "$ARCHIVE" -C "$STAGING_DIR"

PACKAGE_NAME=$(basename "$ARCHIVE" .tar.gz)
PACKAGE_ROOT="$STAGING_DIR/$PACKAGE_NAME"
if [ ! -x "$PACKAGE_ROOT/install.sh" ]; then
  echo "weft-codex install: archive does not contain an executable install.sh" >&2
  exit 1
fi

"$PACKAGE_ROOT/install.sh"
