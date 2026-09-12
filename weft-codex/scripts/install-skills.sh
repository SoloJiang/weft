#!/bin/sh
# Sync weft-codex product skills into the user's Codex home.
#
# Preferred path: `weft-codex doctor` / `weft-codex` / package install.sh already
# refresh managed skills from the runtime package when their version changes. This script remains for
# source checkouts and explicit force reinstalls.
#
#   scripts/install-skills.sh            # install/update managed skills
#   scripts/install-skills.sh --force    # overwrite local non-managed copies
#   CODEX_HOME=/tmp/x scripts/install-skills.sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

FORCE_ARGS=
for arg in "$@"; do
  case "$arg" in
    --force) FORCE_ARGS=--force ;;
    *)
      echo "install-skills: unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

if [ -x "$PROJECT_ROOT/launcher/dist/cli.js" ] || [ -f "$PROJECT_ROOT/launcher/src/cli.ts" ]; then
  if [ ! -d "$PROJECT_ROOT/launcher/node_modules" ]; then
    pnpm --dir "$PROJECT_ROOT/launcher" install --frozen-lockfile >/dev/null
  fi
  if [ ! -d "$PROJECT_ROOT/launcher/dist" ]; then
    pnpm --dir "$PROJECT_ROOT/launcher" build >/dev/null
  fi
  exec node "$PROJECT_ROOT/launcher/dist/cli.js" install-skills \
    --skills-dir="$PROJECT_ROOT/skills" \
    ${FORCE_ARGS}
fi

if command -v weft-codex >/dev/null 2>&1; then
  exec weft-codex install-skills --skills-dir="$PROJECT_ROOT/skills" ${FORCE_ARGS}
fi

echo "install-skills: neither launcher build output nor weft-codex is available" >&2
exit 1
