import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  targets,
  developmentRegionFallback,
} from "../../scripts/gen-infoplist-strings.mjs";

/** The catalogs are authoritative; the shipped `InfoPlist.strings` files (and
 *  the development-region fallback embedded in `src-tauri/Info.plist`) are
 *  mirrors macOS renders the Automation consent prompt from. If they drift,
 *  the OS prompt and the catalogs say different sentences — the duplication
 *  the generation step exists to prevent. */
test("the generated InfoPlist.strings mirrors match the catalogs", () => {
  for (const { relPath, content } of targets()) {
    const onDisk = readFileSync(new URL(`../../scripts/${relPath}`, import.meta.url), "utf8");
    assert.equal(
      onDisk,
      content,
      `${relPath} is stale — run \`node --experimental-strip-types scripts/gen-infoplist-strings.mjs\``,
    );
  }
});

test("Info.plist's fallback usage description equals the English catalog copy", () => {
  const plist = readFileSync(new URL("../../src-tauri/Info.plist", import.meta.url), "utf8");
  const match = plist.match(
    /<key>NSAppleEventsUsageDescription<\/key>\s*<string>([\s\S]*?)<\/string>/,
  );
  assert.ok(match, "Info.plist must declare NSAppleEventsUsageDescription");
  const embedded = match[1]
    .replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
  assert.equal(
    embedded,
    developmentRegionFallback(),
    "src-tauri/Info.plist's embedded NSAppleEventsUsageDescription drifted from " +
      "src/i18n/en.ts (settings.computerUseMacosAutomationPrompt) — edit the catalog, " +
      "then mirror the same sentence into Info.plist",
  );
});
