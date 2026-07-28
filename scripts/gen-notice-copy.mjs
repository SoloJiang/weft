// Generate the backend-readable mirror of the notice copy that lives in the
// i18n catalogs.
//
// `src/i18n/{en,zh}.ts` stay authoritative (AGENTS.md: user-facing strings go
// only through those two). Some notices are posted by detached backend tasks
// that have no locale, so they emit a stable TOKEN and each consumer renders
// it; the webview reads its catalogs directly, while the IM bridge is Rust and
// cannot. This writes what Rust bakes in.
//
// Run after editing any listed key. `tests/frontend/noticeCopy.test.ts` fails
// if the generated file drifts from the catalogs, so it cannot go stale
// unnoticed.
import { writeFileSync } from "node:fs";
import { en } from "../src/i18n/en.ts";
import { zh } from "../src/i18n/zh.ts";

/** Backend token -> the catalog key holding its copy. */
const TOKENS = {
  "acp.force_reset_notice": "needs.acpForceResetNotice",
};

const OUT = new URL("../src-tauri/src/bus/notices.generated.json", import.meta.url);

function lookup(catalog, key) {
  return key.split(".").reduce((node, part) => node?.[part], catalog);
}

export function buildNoticeCopy() {
  const out = {};
  for (const [token, key] of Object.entries(TOKENS)) {
    const enCopy = lookup(en, key);
    const zhCopy = lookup(zh, key);
    if (typeof enCopy !== "string" || typeof zhCopy !== "string") {
      throw new Error(`${token}: ${key} is missing from en or zh`);
    }
    out[token] = { en: enCopy, zh: zhCopy };
  }
  return out;
}

export function serialize(copy) {
  return `${JSON.stringify(copy, null, 2)}\n`;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  writeFileSync(OUT, serialize(buildNoticeCopy()));
  console.log(`wrote ${OUT.pathname}`);
}
