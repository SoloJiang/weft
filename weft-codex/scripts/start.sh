#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

if [ ! -d "$PROJECT_ROOT/ui/node_modules" ]; then
  pnpm --dir "$PROJECT_ROOT/ui" install --frozen-lockfile
fi
if [ ! -d "$PROJECT_ROOT/launcher/node_modules" ]; then
  pnpm --dir "$PROJECT_ROOT/launcher" install --frozen-lockfile
fi

pnpm --dir "$PROJECT_ROOT/ui" build
pnpm --dir "$PROJECT_ROOT/launcher" build
cargo build --manifest-path "$PROJECT_ROOT/Cargo.toml" -p weftd

exec node "$PROJECT_ROOT/launcher/dist/cli.js" "$@"
