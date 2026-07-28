import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { buildNoticeCopy, serialize } from "../../scripts/gen-notice-copy.mjs";

const GENERATED = new URL(
  "../../src-tauri/src/bus/notices.generated.json",
  import.meta.url,
);

/** The catalogs are authoritative; the generated file is a mirror the IM
 *  bridge renders from. If they drift, remote and in-app users see different
 *  sentences — the exact duplication this generation step replaced. */
test("the generated notice mirror matches the catalogs", () => {
  const onDisk = readFileSync(GENERATED, "utf8");
  const expected = serialize(buildNoticeCopy());
  assert.equal(
    onDisk,
    expected,
    "src-tauri/src/bus/notices.generated.json is stale — run `node --experimental-strip-types scripts/gen-notice-copy.mjs`",
  );
});
