#!/bin/sh
set -eu

PACKAGE_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PREFIX_INPUT=${WEFT_CODEX_PREFIX:-}
if [ -z "$PREFIX_INPUT" ]; then
  USER_HOME=$(CDPATH= cd -- && pwd)
  PREFIX_INPUT="$USER_HOME/.local"
fi

case "$PREFIX_INPUT" in
  /*) ;;
  *)
    echo "weft-codex install: WEFT_CODEX_PREFIX must be an absolute path" >&2
    exit 1
    ;;
esac

for required_path in \
  bin/weft-codex \
  bin/weftd \
  share/weft-codex/web/index.html \
  share/weft-codex/skills/weft-derive-test-cases/SKILL.md \
  libexec/weft-codex-wrapper
do
  if [ ! -e "$PACKAGE_ROOT/$required_path" ]; then
    echo "weft-codex install: package is incomplete: $required_path" >&2
    exit 1
  fi
done

mkdir -p "$PREFIX_INPUT/bin" "$PREFIX_INPUT/share/weft-codex/releases"
PREFIX=$(CDPATH= cd -- "$PREFIX_INPUT" && pwd)
RUNTIME_BASE="$PREFIX/share/weft-codex"
RELEASES_DIR="$RUNTIME_BASE/releases"
PACKAGE_NAME=$(basename "$PACKAGE_ROOT")
PACKAGE_DIGEST=$(shasum -a 256 "$PACKAGE_ROOT/bin/weft-codex" | awk '{print substr($1, 1, 12)}')
RELEASE_ID="$PACKAGE_NAME-$PACKAGE_DIGEST"
RELEASE_ROOT="$RELEASES_DIR/$RELEASE_ID"
STAGING_DIR=

cleanup() {
  if [ -n "$STAGING_DIR" ] && [ -d "$STAGING_DIR" ]; then
    rm -rf "$STAGING_DIR"
  fi
}
trap cleanup EXIT HUP INT TERM

if [ ! -d "$RELEASE_ROOT" ]; then
  STAGING_DIR=$(mktemp -d "$RELEASES_DIR/.install.XXXXXX")
  cp -R "$PACKAGE_ROOT/." "$STAGING_DIR/"
  if [ ! -x "$STAGING_DIR/bin/weft-codex" ] || [ ! -x "$STAGING_DIR/bin/weftd" ]; then
    echo "weft-codex install: staged runtime validation failed" >&2
    exit 1
  fi
  mv "$STAGING_DIR" "$RELEASE_ROOT"
  STAGING_DIR=
fi

ln -sfn "releases/$RELEASE_ID" "$RUNTIME_BASE/current"
install -m 755 "$RELEASE_ROOT/libexec/weft-codex-wrapper" "$PREFIX/bin/weft-codex"

# doctor also refreshes product skills into $CODEX_HOME/skills so upgrades
# carry the runtime's SKILL.md without a separate install step.
"$PREFIX/bin/weft-codex" doctor >/dev/null

echo "Installed weft-codex:"
echo "  command: $PREFIX/bin/weft-codex"
echo "  runtime: $RELEASE_ROOT"
echo "  skills:  synced into ${CODEX_HOME:-$HOME/.codex}/skills on doctor/start"
case ":$PATH:" in
  *":$PREFIX/bin:"*) ;;
  *) echo "  PATH: add $PREFIX/bin to your shell PATH" ;;
esac
