import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Layers, User } from "lucide-react";
import type { SwitchOutcome, ToolStatus } from "../lib/types";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Button } from "../components/ui/Button";
import { Field, Input } from "../components/ui/Input";
import { Segmented } from "../components/ui/Segmented";
import { ToolIcon, toolFullName } from "../components/ToolIcon";
import { modelSupported, switchKindOf } from "./engineSwitch";

/** Which layer this dialog changes (issue #96 pitfall #4: "switching the lead
 *  ≠ switching a worker ≠ changing the global default" — the dogfooding
 *  report's concrete harm was mistaking one for another). Always explicit,
 *  never inferred from context, so the scope banner below can never drift
 *  from what the confirm button actually calls. */
export type SwitchScope = "lead" | "worker";

const SCOPE_COPY: Record<SwitchScope, { icon: typeof Layers; banner: string; title: string }> = {
  lead: { icon: Layers, banner: "session.switchScopeLead", title: "session.switchTitleLead" },
  worker: { icon: User, banner: "session.switchScopeWorker", title: "session.switchTitleWorker" },
};

/**
 * Confirm dialog for an engine/model switch (issue #96/#98). Same shape as
 * RewindDialog (open state driven by the parent, busy/err inside): pick a
 * tool + optional model override, confirm, and the backend does the rest
 * (clear native id, tear down + rebuild the engine, inject a history digest,
 * leave a durable timeline marker). The scope banner is the ONE place the
 * "which layer am I changing" answer lives — it is derived from `scope`
 * alone, never re-guessed from the tool/model values.
 */
export function EngineSwitchDialog({
  open,
  onOpenChange,
  scope,
  currentTool,
  currentModel,
  installedTools,
  onConfirm,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  scope: SwitchScope;
  currentTool: string;
  currentModel: string | null;
  installedTools: ToolStatus[];
  onConfirm: (tool: string, model: string | null) => Promise<SwitchOutcome>;
}) {
  const { t } = useTranslation();
  const [tool, setTool] = useState(currentTool);
  const [model, setModel] = useState(currentModel ?? "");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open && !wasOpen.current) {
      setTool(currentTool);
      setModel(currentModel ?? "");
      setBusy(false);
      setErr(null);
    }
    wasOpen.current = open;
  }, [open, currentTool, currentModel]);

  const installed = installedTools.filter((tl) => tl.installed);
  const modelOk = modelSupported(tool);
  const isReload = switchKindOf(currentTool, tool) === "reload";

  async function confirm() {
    if (busy) return;
    setBusy(true);
    setErr(null);
    try {
      await onConfirm(tool, modelOk ? model.trim() || null : null);
      onOpenChange(false);
    } catch (e) {
      setErr(String(e));
      if (import.meta.env.DEV) console.error("engine switch failed:", String(e));
      setBusy(false);
    }
  }

  const copy = SCOPE_COPY[scope];
  const Icon = copy.icon;
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t(copy.title)}>
        <div className="flex flex-col gap-4">
          <div className="flex items-start gap-2 rounded-[var(--radius-md)] border border-border bg-bg px-3 py-2.5">
            <Icon size={14} className="mt-0.5 shrink-0 text-brand" />
            <p className="text-[12px] leading-relaxed text-ink-muted">{t(copy.banner)}</p>
          </div>

          <Field label={t("session.switchToolLabel")}>
            {installed.length === 0 ? (
              <span className="text-[12px] text-waiting">{t("settings.noTools")}</span>
            ) : (
              <Segmented
                value={tool}
                onChange={setTool}
                options={installed.map((tl) => ({
                  value: tl.tool,
                  label: toolFullName(tl.tool),
                  icon: <ToolIcon tool={tl.tool} size={12} />,
                }))}
              />
            )}
          </Field>

          <Field
            label={t("session.switchModelLabel")}
            hint={modelOk ? t("session.switchModelHint") : t("session.switchModelUnsupported", { tool: toolFullName(tool) })}
          >
            <Input
              value={modelOk ? model : ""}
              onChange={(e) => setModel(e.currentTarget.value)}
              disabled={!modelOk}
              placeholder={t("session.switchModelPlaceholder")}
              className="font-mono"
            />
          </Field>

          <p className="text-[12px] leading-relaxed text-ink-muted">
            {t(isReload ? "session.switchReloadBody" : "session.switchBody", {
              tool: toolFullName(tool),
            })}
          </p>
          {err && <p className="text-[12px] text-danger">{err}</p>}

          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" disabled={busy} onClick={() => onOpenChange(false)}>
              {t("common.cancel")}
            </Button>
            <Button type="button" variant="primary" disabled={busy} onClick={() => void confirm()}>
              {t(isReload ? "session.switchReloadConfirm" : "session.switchConfirm")}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}
