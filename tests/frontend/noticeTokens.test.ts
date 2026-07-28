import test from "node:test";
import assert from "node:assert/strict";
import { NOTICE_TOKENS, noticeTokenKey } from "../../src/lib/noticeTokens.ts";
import { en } from "../../src/i18n/en.ts";
import { zh } from "../../src/i18n/zh.ts";

/** Walk a dotted i18n key ("needs.acpForceResetNotice") through a catalog. */
function lookup(catalog: unknown, key: string): unknown {
  return key
    .split(".")
    .reduce<unknown>(
      (node, part) =>
        node && typeof node === "object" ? (node as Record<string, unknown>)[part] : undefined,
      catalog,
    );
}

test("every notice token resolves to real copy in BOTH catalogs", () => {
  const keys = Object.values(NOTICE_TOKENS);
  assert.ok(keys.length > 0, "the map should not be empty");
  for (const key of keys) {
    for (const [name, catalog] of [
      ["en", en],
      ["zh", zh],
    ] as const) {
      const value = lookup(catalog, key);
      assert.equal(typeof value, "string", `${name} is missing ${key}`);
      assert.ok((value as string).length > 0, `${name}'s ${key} is empty`);
    }
  }
});

/** The backend pins the same literal (engine::ACP_FORCE_RESET_NOTICE). If the
 *  two drift, the raw token renders as the notice body instead of the copy. */
test("the force-reset token matches the one the backend emits", () => {
  assert.equal(noticeTokenKey("acp.force_reset_notice"), "needs.acpForceResetNotice");
});

test("prose passes through untouched", () => {
  assert.equal(noticeTokenKey("Should I bump the major version?"), null);
  assert.equal(noticeTokenKey(""), null);
});

/** Rust bakes this same file in at compile time (`bus::notice_text`). If the
 *  catalogs stopped sourcing from it, in-app and IM copy would drift — which is
 *  exactly the duplication this file replaced. */
test("catalog copy comes from the shared notices file, not a second transcription", async () => {
  const notices = (await import("../../src/i18n/notices.json", { with: { type: "json" } }))
    .default as Record<string, Record<string, string>>;
  const shared = notices["acp.force_reset_notice"];
  assert.ok(shared, "the shared file must carry the force-reset token");
  assert.equal(lookup(en, "needs.acpForceResetNotice"), shared.en);
  assert.equal(lookup(zh, "needs.acpForceResetNotice"), shared.zh);
});
