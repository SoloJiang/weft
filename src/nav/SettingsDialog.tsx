import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, FolderOpen, Loader2, Moon, Sun, Zap } from "lucide-react";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Field, Input } from "../components/ui/Input";
import { ToolIcon } from "../components/ToolIcon";
import { useTheme } from "../state/theme";
import { setLang, currentLang, type Lang } from "../i18n";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { ToolStatus } from "../lib/types";
import { cn } from "../lib/cn";

const TOOLS = ["claude", "codex", "opencode"] as const;
const TOOL_LABEL: Record<string, string> = { claude: "Claude", codex: "Codex", opencode: "OpenCode" };

/** App-level settings: appearance, language, projects directory, default tool. */
export function SettingsDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
}) {
  const { t } = useTranslation();
  const { theme, toggle } = useTheme();
  const { projectsDir, setProjectsDir, defaultTool, setDefaultTool, dangerousMode, setDangerousMode } =
    useStore();
  const [lang, setLangState] = useState<Lang>(currentLang());
  const [detected, setDetected] = useState<ToolStatus[] | null>(null);

  // Detect installed CLIs each time the dialog opens.
  useEffect(() => {
    if (!open) return;
    setDetected(null);
    let alive = true;
    void api.detectTools().then((d) => alive && setDetected(d));
    return () => {
      alive = false;
    };
  }, [open]);

  const statusOf = (tool: string) => detected?.find((d) => d.tool === tool);

  async function pickDir() {
    const dir = await api.pickFolder(t("settings.projectsDir"));
    if (dir) setProjectsDir(dir);
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t("settings.title")} description={t("settings.subtitle")}>
        <div className="flex flex-col gap-5">
          {/* appearance */}
          <Row label={t("settings.appearance")}>
            <Segmented
              value={theme}
              onChange={(v) => {
                if (v !== theme) toggle();
              }}
              options={[
                { value: "light", label: t("settings.light"), icon: <Sun size={13} /> },
                { value: "dark", label: t("settings.dark"), icon: <Moon size={13} /> },
              ]}
            />
          </Row>

          {/* language */}
          <Row label={t("settings.language")}>
            <Segmented
              value={lang}
              onChange={(v) => {
                setLang(v as Lang);
                setLangState(v as Lang);
              }}
              options={[
                { value: "zh", label: "中文" },
                { value: "en", label: "English" },
              ]}
            />
          </Row>

          <div className="h-px bg-border" />

          {/* projects directory */}
          <Field label={t("settings.projectsDir")} hint={t("settings.projectsDirHint")}>
            <div className="flex items-center gap-2">
              <Input
                value={projectsDir}
                placeholder="/Users/you/code"
                onChange={(e) => setProjectsDir(e.currentTarget.value)}
              />
              <button
                type="button"
                onClick={() => void pickDir()}
                title={t("settings.choose")}
                className="grid h-9 w-9 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border text-ink-muted transition-colors hover:bg-surface hover:text-ink"
              >
                <FolderOpen size={15} />
              </button>
            </div>
          </Field>

          {/* default coding tool */}
          <div className="flex flex-col gap-2">
            <div className="flex items-center justify-between">
              <span className="text-[12px] font-medium text-ink">{t("settings.defaultTool")}</span>
              {detected === null && (
                <span className="flex items-center gap-1 text-[11px] text-ink-faint">
                  <Loader2 size={11} className="animate-spin" />
                  {t("settings.detecting")}
                </span>
              )}
            </div>
            <div className="flex flex-col gap-1.5">
              {TOOLS.map((tool) => {
                const st = statusOf(tool);
                const installed = st?.installed ?? false;
                const active = defaultTool === tool;
                return (
                  <button
                    key={tool}
                    type="button"
                    onClick={() => setDefaultTool(tool)}
                    className={cn(
                      "flex items-center gap-2.5 rounded-[var(--radius-md)] border px-3 py-2 text-left transition-colors",
                      active
                        ? "border-brand/50 bg-brand-ghost"
                        : "border-border hover:bg-surface",
                    )}
                  >
                    <ToolIcon tool={tool} size={15} />
                    <span className="text-[13px] font-medium text-ink">{TOOL_LABEL[tool]}</span>
                    {detected !== null &&
                      (installed ? (
                        <span className="flex items-center gap-1 text-[11px] text-running">
                          <span className="h-1.5 w-1.5 rounded-full bg-running" />
                          {st?.version
                            ? t("settings.installedVersion", { version: trimVersion(st.version) })
                            : t("settings.installed")}
                        </span>
                      ) : (
                        <span className="text-[11px] text-ink-faint">{t("settings.notInstalled")}</span>
                      ))}
                    <span className="ml-auto">
                      {active && <Check size={15} className="text-brand" />}
                    </span>
                  </button>
                );
              })}
            </div>
            <p className="text-[11px] leading-relaxed text-ink-faint">{t("settings.defaultToolHint")}</p>
          </div>

          <div className="h-px bg-border" />

          {/* dangerous mode */}
          <div className="flex items-start gap-3">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5">
                <Zap size={13} className="text-waiting" />
                <span className="text-[12px] font-medium text-ink">{t("settings.dangerTitle")}</span>
              </div>
              <p className="mt-1 text-[11px] leading-relaxed text-ink-faint">
                {t("settings.dangerDesc")}
              </p>
            </div>
            <Toggle on={dangerousMode} onChange={setDangerousMode} label={t("settings.dangerTitle")} />
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function Toggle({
  on,
  onChange,
  label,
}: {
  on: boolean;
  onChange: (v: boolean) => void;
  label: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label}
      onClick={() => onChange(!on)}
      className={cn(
        "mt-0.5 inline-flex h-5 w-9 shrink-0 items-center rounded-full border-0 p-0 transition-colors",
        on ? "bg-waiting" : "bg-border-strong",
      )}
    >
      <span
        className={cn(
          "inline-block h-4 w-4 rounded-full bg-white shadow transition-transform duration-150",
          on ? "translate-x-[18px]" : "translate-x-0.5",
        )}
      />
    </button>
  );
}

/** Keep version strings short (e.g. "1.2.3 (Claude Code)" -> "1.2.3"). */
function trimVersion(v: string): string {
  const m = v.match(/\d+\.\d+\.\d+/);
  return m ? m[0] : v.split(/\s+/)[0];
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4">
      <span className="text-[12px] font-medium text-ink">{label}</span>
      {children}
    </div>
  );
}

function Segmented({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string; icon?: React.ReactNode }[];
}) {
  return (
    <div className="flex items-center rounded-[var(--radius-md)] bg-bg p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cn(
            "flex items-center gap-1.5 rounded-[var(--radius-sm)] px-2.5 py-1 text-[12px] transition-colors",
            value === o.value
              ? "bg-raised text-ink shadow-[0_1px_2px_rgba(0,0,0,0.2)]"
              : "text-ink-faint hover:text-ink-muted",
          )}
        >
          {o.icon}
          {o.label}
        </button>
      ))}
    </div>
  );
}
