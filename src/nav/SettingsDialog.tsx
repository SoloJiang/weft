import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  ArrowLeft,
  Bot,
  FolderOpen,
  Moon,
  Palette,
  Search,
  Settings,
  Sun,
} from "lucide-react";
import { Input } from "../components/ui/Input";
import { toolFullName } from "../components/ToolIcon";
import { currentLang, setLang, type Lang } from "../i18n";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import { useStore } from "../state/store";
import { useTheme } from "../state/theme";

type SettingsPage = "general" | "appearance" | "automation";

type NavItem = {
  id: SettingsPage;
  icon: typeof Settings;
  labelKey: string;
  implemented?: boolean;
};

const NAV_GROUPS: { labelKey: string; items: NavItem[] }[] = [
  {
    labelKey: "settings.groupPersonal",
    items: [
      { id: "general", icon: Settings, labelKey: "settings.general", implemented: true },
      { id: "appearance", icon: Palette, labelKey: "settings.appearance", implemented: true },
      { id: "automation", icon: Bot, labelKey: "settings.automation", implemented: true },
    ],
  },
];

export function SettingsScreen() {
  const { t } = useTranslation();
  const { setHomeTab } = useStore();
  const [active, setActive] = useState<SettingsPage>("general");
  const [query, setQuery] = useState("");

  const groups = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return NAV_GROUPS;
    return NAV_GROUPS.map((group) => ({
      ...group,
      items: group.items.filter((item) => t(item.labelKey).toLowerCase().includes(q)),
    })).filter((group) => group.items.length > 0);
  }, [query, t]);

  const activeLabel = t(NAV_GROUPS.flatMap((group) => group.items).find((item) => item.id === active)?.labelKey ?? "settings.general");

  return (
    <section className="flex h-screen w-screen overflow-hidden bg-bg text-ink">
      <aside className="flex w-80 shrink-0 flex-col border-r border-border bg-surface">
        <div className="px-3 pb-3 pt-5">
          <button
            type="button"
            onClick={() => setHomeTab("board")}
            className="mb-4 flex items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-[13px] text-ink-muted transition-colors hover:bg-brand-ghost hover:text-ink"
          >
            <ArrowLeft size={15} />
            {t("settings.backToApp")}
          </button>
          <div className="relative">
            <Search size={14} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-ink-faint" />
            <input
              value={query}
              onChange={(e) => setQuery(e.currentTarget.value)}
              placeholder={t("settings.searchPlaceholder")}
              className="h-8 w-full rounded-[var(--radius-md)] border border-border bg-bg pl-8 pr-2 text-[13px] text-ink outline-none placeholder:text-ink-faint transition-colors hover:border-border-strong focus:border-brand focus:ring-2 focus:ring-brand/25"
            />
          </div>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-4">
          {groups.map((group) => (
            <div key={group.labelKey} className="mb-5">
              <div className="px-2 pb-1.5 text-[12px] font-medium text-ink-faint">
                {t(group.labelKey)}
              </div>
              <div className="grid gap-0.5">
                {group.items.map((item) => (
                  <SettingsNavButton
                    key={item.id}
                    item={item}
                    active={active === item.id}
                    onClick={() => setActive(item.id)}
                  />
                ))}
              </div>
            </div>
          ))}
        </div>
      </aside>

      <main className="min-w-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[760px] px-8 pb-16 pt-16">
          <h1 className="text-[22px] font-semibold tracking-[-0.01em] text-ink">{activeLabel}</h1>
          <div className="mt-10">
            {active === "general" ? (
              <GeneralSettings />
            ) : active === "appearance" ? (
              <AppearanceSettings />
            ) : (
              <AutomationSettings />
            )}
          </div>
        </div>
      </main>
    </section>
  );
}

function SettingsNavButton({
  item,
  active,
  onClick,
}: {
  item: NavItem;
  active: boolean;
  onClick: () => void;
}) {
  const { t } = useTranslation();
  const Icon = item.icon;
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "flex w-full items-center gap-2 rounded-[var(--radius-md)] px-2 py-1.5 text-left text-[13px] transition-colors",
        active ? "bg-hover text-ink" : "text-ink-muted hover:bg-hover/70 hover:text-ink",
      )}
    >
      <Icon size={15} className={active ? "text-ink" : "text-ink-faint"} />
      <span className="min-w-0 flex-1 truncate">{t(item.labelKey)}</span>
    </button>
  );
}

function GeneralSettings() {
  const { t } = useTranslation();
  const {
    projectsDir,
    setProjectsDir,
    defaultTool,
    setDefaultTool,
    configuredTool,
    installedTools,
    reviewSkill,
    setReviewSkill,
    autoReview,
    setAutoReview,
  } = useStore();
  const [lang, setLangState] = useState<Lang>(currentLang());

  const installed = installedTools.filter((tl) => tl.installed);

  useEffect(() => {
    setLangState(currentLang());
  }, []);

  async function pickDir() {
    const dir = await api.pickFolder(t("settings.projectsDir"));
    if (dir) setProjectsDir(dir);
  }

  return (
    <div className="flex flex-col gap-10">
      <SettingsGroup title={t("settings.defaults")}>
        <SettingRow label={t("settings.defaultTool")} hint={t("settings.defaultToolHint")}>
          {installed.length === 0 ? (
            <span className="text-[12px] text-waiting">{t("settings.noTools")}</span>
          ) : (
            <div className="flex flex-col items-end gap-1">
              <Segmented
                value={defaultTool}
                onChange={setDefaultTool}
                options={installed.map((tl) => ({ value: tl.tool, label: toolFullName(tl.tool) }))}
              />
              {configuredTool && configuredTool !== defaultTool && (
                <span className="text-[11px] text-waiting">
                  {t("settings.toolFallback", {
                    configured: toolFullName(configuredTool),
                    tool: toolFullName(defaultTool),
                  })}
                </span>
              )}
            </div>
          )}
        </SettingRow>
        <SettingRow label={t("settings.projectsDir")} hint={t("settings.projectsDirHint")}>
          <div className="flex w-[360px] max-w-[42vw] items-center gap-2">
            <Input
              value={projectsDir}
              placeholder="/Users/you/code"
              onChange={(e) => setProjectsDir(e.currentTarget.value)}
              className="h-8 min-w-0 bg-bg/80 font-mono text-[12px]"
            />
            <button
              type="button"
              onClick={() => void pickDir()}
              title={t("settings.choose")}
              className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] border border-border bg-bg/80 text-ink-muted transition-colors duration-150 hover:border-border-strong hover:bg-hover hover:text-ink active:bg-raised"
            >
              <FolderOpen size={14} />
            </button>
          </div>
        </SettingRow>
        <SettingRow label={t("settings.reviewSkill")} hint={t("settings.reviewSkillHint")}>
          <Input
            value={reviewSkill}
            placeholder="superpowers:requesting-code-review"
            onChange={(e) => setReviewSkill(e.currentTarget.value)}
            className="h-8 w-[360px] max-w-[42vw] bg-bg/80 font-mono text-[12px]"
          />
        </SettingRow>
        <SettingRow label={t("settings.autoReview")} hint={t("settings.autoReviewHint")}>
          <Toggle on={autoReview} onChange={setAutoReview} label={t("settings.autoReview")} />
        </SettingRow>
        <SettingRow label={t("settings.agentLanguage")} hint={t("settings.agentLanguageHint")}>
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
        </SettingRow>
      </SettingsGroup>
    </div>
  );
}

function AppearanceSettings() {
  const { t } = useTranslation();
  const { theme, toggle } = useTheme();
  const [lang, setLangState] = useState<Lang>(currentLang());

  useEffect(() => {
    setLangState(currentLang());
  }, []);

  return (
    <SettingsGroup title={t("settings.interface")}>
      <SettingRow label={t("settings.theme")}>
        <Segmented
          value={theme}
          onChange={(v) => {
            if (v !== theme) toggle();
          }}
          options={[
            { value: "dark", label: t("settings.dark"), icon: <Moon size={13} /> },
            { value: "light", label: t("settings.light"), icon: <Sun size={13} /> },
          ]}
        />
      </SettingRow>
      <SettingRow label={t("settings.language")} hint={t("settings.languageHint")}>
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
      </SettingRow>
    </SettingsGroup>
  );
}

function AutomationSettings() {
  const { t } = useTranslation();
  const { dangerousMode, setDangerousMode } = useStore();
  const [loopGuard, setLoopGuard] = useState(true);

  return (
    <SettingsGroup title={t("settings.rules")}>
      <SettingRow label={t("settings.dangerTitle")} hint={t("settings.dangerDesc")}>
        <Toggle on={dangerousMode} onChange={setDangerousMode} label={t("settings.dangerTitle")} />
      </SettingRow>
      <SettingRow label={t("settings.loopDetection")} hint={t("settings.loopDetectionHint")}>
        <Toggle on={loopGuard} onChange={setLoopGuard} label={t("settings.loopDetection")} />
      </SettingRow>
    </SettingsGroup>
  );
}

function SettingsGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-3">
      <h2 className="text-[13px] font-semibold text-ink">{title}</h2>
      <div className="flex flex-col rounded-[var(--radius-lg)] border border-border bg-surface [&>div+div]:border-t [&>div+div]:border-border">
        {children}
      </div>
    </section>
  );
}

function SettingRow({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children?: ReactNode;
}) {
  return (
    <div className="flex min-h-[72px] items-center gap-4 px-3 py-3">
      <div className="min-w-0">
        <div className="text-[12.5px] font-semibold text-ink">{label}</div>
        {hint && <p className="mt-1 max-w-[58ch] text-[12px] leading-relaxed text-ink-faint">{hint}</p>}
      </div>
      <span className="min-w-4 flex-1" />
      {children && <div className="shrink-0">{children}</div>}
    </div>
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
        "relative inline-flex h-[22px] w-[38px] shrink-0 items-center rounded-full p-0 transition-colors duration-150",
        on ? "bg-brand" : "bg-border-strong",
      )}
    >
      <span
        className={cn(
          "absolute left-0.5 top-0.5 inline-block h-[18px] w-[18px] rounded-full bg-white shadow-[0_1px_2px_rgba(0,0,0,0.3)] transition-transform duration-150",
          on ? "translate-x-4" : "translate-x-0",
        )}
      />
    </button>
  );
}

function Segmented({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (v: string) => void;
  options: { value: string; label: string; icon?: ReactNode }[];
}) {
  return (
    <div className="inline-flex items-center gap-0.5 rounded-[var(--radius-md)] bg-bg p-0.5">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cn(
            "flex h-[28px] items-center gap-1.5 whitespace-nowrap rounded-[var(--radius-sm)] px-3 text-[12px] font-medium transition-colors duration-150",
            value === o.value ? "bg-raised text-ink" : "text-ink-muted hover:text-ink",
          )}
        >
          {o.icon}
          {o.label}
        </button>
      ))}
    </div>
  );
}
