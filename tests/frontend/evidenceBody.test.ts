import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import ts from "typescript";

const require = createRequire(import.meta.url);
const bodyPath = new URL("../../src/components/EvidenceBody.tsx", import.meta.url);

type EvidenceRow = {
  id: number;
  thread_id: number;
  direction_id: number;
  kind: string;
  source: string;
  source_ref: string;
  observed_at: string;
  revision: string;
  policy_revision: string;
  summary: string;
  payload: string;
  collection_state: string;
  superseded_by: number;
  freshness: "fresh" | "stale" | "unknown";
};

type EvidencePanelState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "error" }
  | { kind: "ready"; rows: EvidenceRow[] };

type EvidenceBodyComponent = (props: {
  state: EvidencePanelState;
}) => ReturnType<typeof createElement>;

type EvidenceBodyModule = {
  EvidenceBody: EvidenceBodyComponent;
  evidencePanelState(
    fetchStatus: "idle" | "loading" | "resolved" | "rejected",
    rows: EvidenceRow[] | null,
  ): EvidencePanelState;
};

let modulePromise: Promise<EvidenceBodyModule> | undefined;

function loadEvidenceBody(): Promise<EvidenceBodyModule> {
  if (modulePromise) {
    return modulePromise;
  }
  modulePromise = (async () => {
    const source = readFileSync(bodyPath, "utf8");
    let output = ts.transpileModule(source, {
      compilerOptions: {
        jsx: ts.JsxEmit.ReactJSX,
        module: ts.ModuleKind.ESNext,
        target: ts.ScriptTarget.ES2020,
      },
    }).outputText;
    for (const specifier of ["react/jsx-runtime", "react-i18next"]) {
      const resolved = pathToFileURL(require.resolve(specifier)).href;
      output = output.replaceAll(`"${specifier}"`, `"${resolved}"`);
    }
    const encoded = Buffer.from(output).toString("base64");
    return (await import(`data:text/javascript;base64,${encoded}`)) as EvidenceBodyModule;
  })();
  return modulePromise;
}

function render(Component: EvidenceBodyComponent, state: EvidencePanelState): string {
  const previousWarn = console.warn;
  console.warn = (...args) => {
    if (String(args[0]).includes("react-i18next:: useTranslation")) {
      return;
    }
    previousWarn(...args);
  };
  try {
    return renderToStaticMarkup(createElement(Component, { state }));
  } finally {
    console.warn = previousWarn;
  }
}

function row(overrides: Partial<EvidenceRow> = {}): EvidenceRow {
  return {
    id: 1,
    thread_id: 1,
    direction_id: 1,
    kind: "verification",
    source: "check_flight",
    source_ref: "api",
    observed_at: "100",
    revision: "abc123",
    policy_revision: "",
    summary: "2/2 checks passed: typecheck, test",
    payload: "{}",
    collection_state: "ok",
    superseded_by: 0,
    freshness: "fresh",
    ...overrides,
  };
}

test("evidencePanelState collapses fetch lifecycle into one discriminated state", async () => {
  const { evidencePanelState } = await loadEvidenceBody();
  assert.deepEqual(evidencePanelState("idle", null), { kind: "idle" });
  assert.deepEqual(evidencePanelState("loading", null), { kind: "loading" });
  assert.deepEqual(evidencePanelState("rejected", null), { kind: "error" });
  assert.deepEqual(evidencePanelState("resolved", [row()]), { kind: "ready", rows: [row()] });
  assert.deepEqual(
    evidencePanelState("resolved", null),
    { kind: "ready", rows: [] },
    "a resolved fetch with no rows yet falls back to an empty ready list, not a crash",
  );
});

test("EvidenceBody renders idle and loading identically as the loading copy", async () => {
  const { EvidenceBody } = await loadEvidenceBody();
  const idleHtml = render(EvidenceBody, { kind: "idle" });
  const loadingHtml = render(EvidenceBody, { kind: "loading" });
  assert.match(idleHtml, /evidence\.loading/);
  assert.match(loadingHtml, /evidence\.loading/);
});

test("EvidenceBody renders the error state distinctly", async () => {
  const { EvidenceBody } = await loadEvidenceBody();
  const html = render(EvidenceBody, { kind: "error" });
  assert.match(html, /evidence\.loadFailed/);
  assert.doesNotMatch(html, /evidence\.loading/);
});

test("EvidenceBody renders an empty ready list as the empty copy", async () => {
  const { EvidenceBody } = await loadEvidenceBody();
  const html = render(EvidenceBody, { kind: "ready", rows: [] });
  assert.match(html, /evidence\.empty/);
});

test("EvidenceBody renders every evidence kind and freshness discriminator", async () => {
  const { EvidenceBody } = await loadEvidenceBody();
  const kinds = [
    "code",
    "verification",
    "interface",
    "host",
    "execution",
    "decision",
    "handoff",
  ] as const;
  const freshnesses = ["fresh", "stale", "unknown"] as const;
  const rows = kinds.map((kind, i) =>
    row({ id: i + 1, kind, freshness: freshnesses[i % freshnesses.length] }),
  );
  const html = render(EvidenceBody, { kind: "ready", rows });

  for (const kind of kinds) {
    assert.match(html, new RegExp(`evidence\\.kind\\.${kind}`));
  }
  for (const freshness of freshnesses) {
    assert.match(html, new RegExp(`evidence\\.freshness\\.${freshness}`));
  }
});

test("EvidenceBody shows the source and source_ref, and a superseded badge only when superseded", async () => {
  const { EvidenceBody } = await loadEvidenceBody();
  const supersededHtml = render(EvidenceBody, {
    kind: "ready",
    rows: [row({ source: "reconciliation", source_ref: "widgets", superseded_by: 42 })],
  });
  assert.match(supersededHtml, /reconciliation/);
  assert.match(supersededHtml, /widgets/);
  assert.match(supersededHtml, /evidence\.supersededBadge/);

  const freshHtml = render(EvidenceBody, {
    kind: "ready",
    rows: [row({ superseded_by: 0 })],
  });
  assert.doesNotMatch(freshHtml, /evidence\.supersededBadge/);
});

test("EvidenceBody omits the summary line only when the summary is empty", async () => {
  const { EvidenceBody } = await loadEvidenceBody();
  const withSummary = render(EvidenceBody, {
    kind: "ready",
    rows: [row({ summary: "drifted: expected main, observed feature/x" })],
  });
  assert.match(withSummary, /drifted: expected main, observed feature\/x/);

  const withoutSummary = render(EvidenceBody, {
    kind: "ready",
    rows: [row({ summary: "" })],
  });
  assert.doesNotMatch(withoutSummary, /drifted/);
});
