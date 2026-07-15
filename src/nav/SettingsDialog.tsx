import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { relaunch } from "@tauri-apps/plugin-process";
import {
  ArrowLeft,
  Bot,
  Boxes,
  Check,
  Copy,
  Database,
  ExternalLink,
  FolderOpen,
  KeyRound,
  MessageSquare,
  Monitor,
  Moon,
  Palette,
  QrCode,
  Search,
  Settings,
  Shield,
  Sun,
} from "lucide-react";
import { Button } from "../components/ui/Button";
import { Dialog, DialogContent } from "../components/ui/Dialog";
import { Input } from "../components/ui/Input";
import { Toggle } from "../components/ui/Toggle";
import { SkillsSettings } from "../components/SkillsSettings";
import { BackupSettings } from "../settings/Backup";
import { toolFullName } from "../components/ToolIcon";
import { currentLang, setLang, type Lang } from "../i18n";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import {
  ensureNotifyPermission,
  notifyPermission,
  openSystemNotificationSettings,
  type NotifyPermission,
} from "../lib/notifications";
import { useStore } from "../state/store";
import { useTheme, type ThemePref } from "../state/theme";

type SettingsPage = "general" | "appearance" | "automation" | "skills" | "im" | "backup";

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
  {
    labelKey: "settings.groupIntegrations",
    items: [
      { id: "skills", icon: Boxes, labelKey: "settings.skills", implemented: true },
      { id: "im", icon: MessageSquare, labelKey: "settings.im", implemented: true },
      { id: "backup", icon: Database, labelKey: "settings.backup", implemented: true },
    ],
  },
];

export function SettingsScreen() {
  const { t } = useTranslation();
  const { closeSettings } = useStore();
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
            onClick={closeSettings}
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
          <div className="mt-10">{renderSettingsPage(active)}</div>
        </div>
      </main>
    </section>
  );
}

function renderSettingsPage(active: SettingsPage) {
  switch (active) {
    case "general":
      return <GeneralSettings />;
    case "appearance":
      return <AppearanceSettings />;
    case "automation":
      return <AutomationSettings />;
    case "im":
      return <ImSettings />;
    case "backup":
      return <BackupSettings />;
    case "skills":
      return <SkillsSettings />;
  }
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
    refreshInstalledTools,
    refreshDefaultTool,
    notifyEnabled,
    setNotifyEnabled,
  } = useStore();
  const [lang, setLangState] = useState<Lang>(currentLang());

  const installed = installedTools.filter((tl) => tl.installed);

  // Per-tool command overrides ("aliases", e.g. claude → cc-claude). `draft`
  // holds in-progress edits; `saved` is what the backend persisted, so a Save
  // affordance only shows for a changed row. `applyToExisting` chooses whether a
  // newly-saved alias also retargets sessions created before now.
  const [savedCommands, setSavedCommands] = useState<Record<string, string>>({});
  const [draftCommands, setDraftCommands] = useState<Record<string, string>>({});
  const [applyToExisting, setApplyToExisting] = useState(true);
  useEffect(() => {
    void api.getToolCommands().then((m) => {
      setSavedCommands(m);
      setDraftCommands(m);
    });
  }, []);
  async function saveToolCommand(tool: string) {
    const value = (draftCommands[tool] ?? "").trim();
    await api.setToolCommand(tool, value, applyToExisting);
    const m = await api.getToolCommands();
    setSavedCommands(m);
    setDraftCommands(m);
    // Re-probe so diagnostics reflect the aliased binary's install status, and
    // re-resolve the default tool (an alias can change which tool is available).
    await refreshInstalledTools();
    await refreshDefaultTool();
  }

  // OS notification permission, re-queried every time Settings opens — the
  // user may have just flipped it in the system pane.
  const [notifyPerm, setNotifyPerm] = useState<NotifyPermission | null>(null);
  useEffect(() => {
    void notifyPermission().then(setNotifyPerm);
  }, []);
  const onNotifyToggle = (on: boolean) => {
    setNotifyEnabled(on);
    // Turning it on is the contextual moment to ask the OS (prompt-state only).
    if (on) void ensureNotifyPermission().then(setNotifyPerm);
  };

  useEffect(() => {
    setLangState(currentLang());
  }, []);

  // Encryption state
  const [encrypted, setEncrypted] = useState<boolean | null>(null);
  const [encModal, setEncModal] = useState<"enable" | "disable" | "change" | null>(null);
  const [encPassword, setEncPassword] = useState("");
  const [encConfirm, setEncConfirm] = useState("");
  const [encCurrentPassword, setEncCurrentPassword] = useState("");
  const [encBusy, setEncBusy] = useState(false);
  const [encError, setEncError] = useState("");

  useEffect(() => {
    void api.dbEncryptionStatus().then((s) => setEncrypted(s.encrypted));
  }, []);

  function resetEncForm() {
    setEncPassword("");
    setEncConfirm("");
    setEncCurrentPassword("");
    setEncError("");
  }

  function openEncModal(mode: "enable" | "disable" | "change") {
    resetEncForm();
    setEncModal(mode);
  }

  async function submitEncryption() {
    setEncError("");
    if (!encPassword.trim()) {
      setEncError(t("settings.encryptionPasswordEmpty"));
      return;
    }
    if (encModal === "enable" || encModal === "change") {
      if (encPassword !== encConfirm) {
        setEncError(t("settings.encryptionPasswordMismatch"));
        return;
      }
    }
    setEncBusy(true);
    try {
      if (encModal === "enable") {
        await api.dbEnableEncryption(encPassword);
      } else if (encModal === "disable") {
        await api.dbDisableEncryption(encPassword);
      } else if (encModal === "change") {
        await api.dbChangePassword(encCurrentPassword, encPassword);
      }
      setEncModal(null);
      await relaunch();
    } catch (err) {
      setEncError(t("settings.encryptionError", { error: String(err) }));
    } finally {
      setEncBusy(false);
    }
  }

  function encryptionActionLabel(mode: typeof encModal) {
    switch (mode) {
      case "enable":
        return t("settings.enableEncryption");
      case "disable":
        return t("settings.disableEncryption");
      case "change":
      default:
        return t("settings.changePassword");
    }
  }

  const encDialogTitle = encryptionActionLabel(encModal);
  const encPrimaryLabel = encBusy ? "…" : encDialogTitle;

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
              placeholder={t("settings.projectsDirPlaceholder")}
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
        <SettingRow label={t("settings.notifications")} hint={t("settings.notificationsHint")}>
          <div className="flex flex-col items-end gap-1">
            <Toggle
              on={notifyEnabled}
              onChange={onNotifyToggle}
              label={t("settings.notifications")}
            />
            {notifyEnabled && notifyPerm === "denied" && (
              <button
                type="button"
                onClick={() => void openSystemNotificationSettings()}
                className="text-[11px] text-waiting transition-colors hover:text-ink hover:underline"
              >
                {t("settings.notifyDenied")}
              </button>
            )}
          </div>
        </SettingRow>
        <SettingRow label={t("settings.agentLanguage")} hint={t("settings.agentLanguageHint")}>
          <Segmented
            value={lang}
            onChange={(v) => {
              setLang(v as Lang);
              setLangState(v as Lang);
            }}
            options={[
              { value: "zh", label: t("settings.langZh") },
              { value: "en", label: t("settings.langEn") },
            ]}
          />
        </SettingRow>
      </SettingsGroup>
      <SettingsGroup title={t("settings.agentCommands")}>
        <div className="px-3 pt-2 text-[12px] leading-relaxed text-ink-muted">
          {t("settings.agentCommandsHint")}
        </div>
        {installedTools.map((tl) => {
          const saved = savedCommands[tl.tool] ?? "";
          const draft = draftCommands[tl.tool] ?? "";
          const changed = draft.trim() !== saved.trim();
          return (
            <SettingRow key={tl.tool} label={toolFullName(tl.tool)}>
              <div className="flex w-[360px] max-w-[42vw] items-center gap-2">
                <Input
                  value={draft}
                  placeholder={tl.tool}
                  onChange={(e) =>
                    setDraftCommands((m) => ({ ...m, [tl.tool]: e.currentTarget.value }))
                  }
                  className="h-8 min-w-0 bg-bg/80 font-mono text-[12px]"
                />
                <Button
                  variant="default"
                  disabled={!changed}
                  onClick={() => void saveToolCommand(tl.tool)}
                >
                  {t("settings.save")}
                </Button>
              </div>
            </SettingRow>
          );
        })}
        <SettingRow
          label={t("settings.applyToExisting")}
          hint={t("settings.applyToExistingHint")}
        >
          <Toggle
            on={applyToExisting}
            onChange={setApplyToExisting}
            label={t("settings.applyToExisting")}
          />
        </SettingRow>
      </SettingsGroup>
      <SettingsGroup title={t("settings.diagnostics")}>
        <div className="flex flex-col gap-2.5 px-3 py-3">
          {installedTools.map((tl) => (
            <ToolDiagnosticCard key={tl.tool} tool={tl} />
          ))}
          <div className="flex justify-end pt-1">
            <Button variant="default" onClick={() => void refreshInstalledTools()}>
              {t("settings.refreshDiagnostics")}
            </Button>
          </div>
        </div>
      </SettingsGroup>
      <SettingsGroup title={t("settings.security")}>
        <SettingRow label={t("settings.encryption")} hint={t("settings.encryptionHint")}>
          <div className="flex items-center gap-2">
            {encrypted === null ? (
              <span className="text-[12px] text-ink-faint">…</span>
            ) : (
              <span
                className={cn(
                  "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium",
                  encrypted
                    ? "border border-success/30 bg-success/15 text-success"
                    : "border border-border bg-bg text-ink-faint",
                )}
              >
                <Shield size={11} />
                {encrypted ? t("settings.encryptionEnabled") : t("settings.encryptionDisabled")}
              </span>
            )}
            {!encrypted && (
              <Button variant="primary" onClick={() => openEncModal("enable")}>
                {t("settings.enableEncryption")}
              </Button>
            )}
            {encrypted && (
              <Button variant="default" onClick={() => openEncModal("disable")}>
                {t("settings.disableEncryption")}
              </Button>
            )}
            {encrypted && (
              <Button variant="default" onClick={() => openEncModal("change")}>
                {t("settings.changePassword")}
              </Button>
            )}
          </div>
        </SettingRow>
      </SettingsGroup>

      {/* Encryption password modal */}
      <Dialog
        open={encModal !== null}
        onOpenChange={(open) => {
          if (!open) setEncModal(null);
        }}
      >
        <DialogContent title={encDialogTitle}>
          <div className="flex flex-col gap-4">
            {encModal === "enable" && (
              <p className="text-[12px] leading-relaxed text-danger">
                {t("settings.encryptionEnableWarning")}
              </p>
            )}
            {encModal === "disable" && (
              <p className="text-[12px] leading-relaxed text-waiting">
                {t("settings.encryptionDisableWarning")}
              </p>
            )}
            {encModal === "change" && (
              <div className="flex flex-col gap-2">
                <label className="text-[12px] font-medium text-ink">
                  {t("settings.encryptionCurrentPassword")}
                </label>
                <Input
                  type="password"
                  value={encCurrentPassword}
                  onChange={(e) => setEncCurrentPassword(e.currentTarget.value)}
                  className="h-8 bg-bg/80 font-mono text-[12px]"
                />
              </div>
            )}
            <div className="flex flex-col gap-2">
              <label className="text-[12px] font-medium text-ink">
                {encModal === "change"
                  ? t("settings.encryptionNewPassword")
                  : t("settings.encryptionCurrentPassword")}
              </label>
              <Input
                type="password"
                value={encPassword}
                onChange={(e) => setEncPassword(e.currentTarget.value)}
                className="h-8 bg-bg/80 font-mono text-[12px]"
              />
            </div>
            {(encModal === "enable" || encModal === "change") && (
              <div className="flex flex-col gap-2">
                <label className="text-[12px] font-medium text-ink">
                  {t("settings.encryptionConfirmPassword")}
                </label>
                <Input
                  type="password"
                  value={encConfirm}
                  onChange={(e) => setEncConfirm(e.currentTarget.value)}
                  className="h-8 bg-bg/80 font-mono text-[12px]"
                />
              </div>
            )}
            {encError && <p className="text-[12px] text-danger">{encError}</p>}
            <div className="flex justify-end gap-2">
              <Button variant="default" onClick={() => setEncModal(null)}>
                {t("common.cancel")}
              </Button>
              <Button
                variant="primary"
                onClick={() => void submitEncryption()}
                disabled={encBusy}
              >
                {encPrimaryLabel}
              </Button>
            </div>
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}

function ToolDiagnosticCard({ tool }: { tool: import("../lib/types").ToolStatus }) {
  const { t } = useTranslation();
  let status: "error" | "warning" | "ok" = "ok";
  if (!tool.installed) {
    status = "error";
  } else if (!tool.meets_min) {
    status = "warning";
  }

  let color = "text-danger";
  if (status === "ok") {
    color = "text-success";
  } else if (status === "warning") {
    color = "text-waiting";
  }

  return (
    <div className="rounded-[var(--radius-md)] border border-border bg-bg p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="text-[13px] font-medium text-ink">{toolFullName(tool.tool)}</span>
        <span className={cn("text-[11px]", color)}>{t(`settings.diag_${status}`)}</span>
      </div>
      {tool.path && (
        <div className="mt-1 truncate font-mono text-[11px] text-ink-faint">{tool.path}</div>
      )}
      {tool.version && <div className="text-[11px] text-ink-muted">{tool.version}</div>}
      {tool.diagnostics.length > 0 && (
        <ul className="mt-2 flex flex-col gap-1">
          {tool.diagnostics.map((d, i) => (
            <li key={i} className="text-[11px] text-ink-muted">
              • {d.message}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function AppearanceSettings() {
  const { t } = useTranslation();
  const { pref, setPref } = useTheme();
  const [lang, setLangState] = useState<Lang>(currentLang());

  useEffect(() => {
    setLangState(currentLang());
  }, []);

  return (
    <SettingsGroup title={t("settings.interface")}>
      <SettingRow label={t("settings.theme")}>
        <Segmented
          value={pref}
          onChange={(v) => setPref(v as ThemePref)}
          options={[
            { value: "system", label: t("settings.system"), icon: <Monitor size={13} /> },
            { value: "light", label: t("settings.light"), icon: <Sun size={13} /> },
            { value: "dark", label: t("settings.dark"), icon: <Moon size={13} /> },
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
            { value: "zh", label: t("settings.langZh") },
            { value: "en", label: t("settings.langEn") },
          ]}
        />
      </SettingRow>
    </SettingsGroup>
  );
}

function AutomationSettings() {
  const { t } = useTranslation();
  const {
    dangerousMode,
    setDangerousMode,
    keepAwake,
    setKeepAwake,
    reviewSkill,
    setReviewSkill,
    autoReview,
    setAutoReview,
  } = useStore();
  const [loopGuard, setLoopGuard] = useState(true);
  const [remoteStandby, setRemoteStandby] = useState(false);
  const [remoteStandbyLoaded, setRemoteStandbyLoaded] = useState(false);

  useEffect(() => {
    void api.imGetSettings().then((s) => {
      setRemoteStandby(s.remote_standby);
      setRemoteStandbyLoaded(true);
    });
  }, []);

  async function toggleRemoteStandby(on: boolean) {
    const prev = remoteStandby;
    setRemoteStandby(on);
    try {
      await api.imSetRemoteStandby(on);
    } catch (err) {
      setRemoteStandby(prev);
      throw err;
    }
  }

  return (
    <div className="flex flex-col gap-10">
      <SettingsGroup title={t("settings.rules")}>
        <SettingRow label={t("settings.dangerTitle")} hint={t("settings.dangerDesc")}>
          <Toggle on={dangerousMode} onChange={setDangerousMode} label={t("settings.dangerTitle")} />
        </SettingRow>
        <SettingRow label={t("settings.loopDetection")} hint={t("settings.loopDetectionHint")}>
          <Toggle on={loopGuard} onChange={setLoopGuard} label={t("settings.loopDetection")} />
        </SettingRow>
        <SettingRow label={t("settings.keepAwakeTitle")} hint={t("settings.keepAwakeHint")}>
          <Toggle on={keepAwake} onChange={setKeepAwake} label={t("settings.keepAwakeTitle")} />
        </SettingRow>
        <SettingRow label={t("settings.remoteStandby")} hint={t("settings.remoteStandbyHint")}>
          {remoteStandbyLoaded ? (
            <Toggle
              on={remoteStandby}
              onChange={(v) => void toggleRemoteStandby(v)}
              label={t("settings.remoteStandby")}
            />
          ) : (
            <div
              aria-hidden
              className="h-[22px] w-[38px] shrink-0 rounded-full bg-border-strong/40"
            />
          )}
        </SettingRow>
      </SettingsGroup>
      <SettingsGroup title={t("settings.reviewGroup")}>
        <SettingRow label={t("settings.reviewSkill")} hint={t("settings.reviewSkillHint")}>
          <Input
            value={reviewSkill}
            placeholder={t("settings.reviewSkillPlaceholder")}
            onChange={(e) => setReviewSkill(e.currentTarget.value)}
            className="h-8 w-[360px] max-w-[42vw] bg-bg/80 font-mono text-[12px]"
          />
        </SettingRow>
        <SettingRow label={t("settings.autoReview")} hint={t("settings.autoReviewHint")}>
          <Toggle on={autoReview} onChange={setAutoReview} label={t("settings.autoReview")} />
        </SettingRow>
      </SettingsGroup>
    </div>
  );
}

// 飞书自建应用需开通的权限点，与 src-tauri/src/im/feishu 的实际调用一一对应：
// im:message 覆盖 发消息(message.create)/回复(message.reply)/更新卡片(message_card.patch)；
// 表情回复(message_reaction)实测需单独开 im:message.reactions:write_only（仅 im:message
// 不够）；两条 readonly 用于长连接(im.message.receive_v1) 接收单聊与群聊消息。改后端调用面时同步这里。
const FEISHU_SCOPES = [
  "im:message",
  "im:message.reactions:write_only",
  "im:message.p2p_msg:readonly",
  "im:message.group_msg:readonly",
] as const;

function ImSettings() {
  const { t } = useTranslation();
  const [appId, setAppId] = useState("");
  const [savedAppId, setSavedAppId] = useState("");
  const [secret, setSecret] = useState("");
  const [hasSecret, setHasSecret] = useState(false);
  const [bound, setBound] = useState(false);
  const [enabled, setEnabled] = useState(false);
  const [status, setStatus] = useState("disabled");
  const [saving, setSaving] = useState(false);
  const [copied, setCopied] = useState(false);
  const [scanOpen, setScanOpen] = useState(false);
  // 两种接入模式:扫码创建新 PersonalAgent,或手填凭证绑定已有应用/机器人。
  const [mode, setMode] = useState<"scan" | "manual">("scan");
  // The toggles default to `false`. Without this flag we'd render the
  // off-state for one tick before `api.imGetSettings()` resolves, producing
  // a visible "off → on" flash for users whose IM was already enabled.
  const [loaded, setLoaded] = useState(false);
  const copiedTimer = useRef<number | null>(null);

  useEffect(
    () => () => {
      if (copiedTimer.current !== null) clearTimeout(copiedTimer.current);
    },
    [],
  );

  // 一键复制需开通的权限点：换行分隔，方便逐条粘进开放平台「权限管理」搜索框。
  async function copyPerms() {
    try {
      await navigator.clipboard.writeText(FEISHU_SCOPES.join("\n"));
    } catch {
      return; // 剪贴板不可用时静默——按钮不给假反馈。
    }
    setCopied(true);
    if (copiedTimer.current !== null) clearTimeout(copiedTimer.current);
    copiedTimer.current = window.setTimeout(() => setCopied(false), 1500);
  }

  useEffect(() => {
    void api.imGetSettings().then((s) => {
      setAppId(s.app_id);
      setSavedAppId(s.app_id);
      setHasSecret(s.has_secret);
      setBound(s.bound);
      setEnabled(s.enabled);
      setLoaded(true);
    });
    void api.imStatus().then(setStatus);
    const id = setInterval(() => void api.imStatus().then(setStatus), 3000);
    return () => clearInterval(id);
  }, []);

  // 开关 = 启用/断开。乐观翻转：展开/收起即时响应，后台再落库重启桥。
  async function toggle(on: boolean) {
    const prev = enabled;
    setEnabled(on);
    try {
      await api.imSetEnabled(on);
      void api.imStatus().then(setStatus);
    } catch (err) {
      setEnabled(prev);
      throw err;
    }
  }

  // 已连接卡片常驻展开，所以编辑即就地改；有未提交改动才点亮「重新连接」。
  const dirty = appId.trim() !== savedAppId.trim() || secret.length > 0;

  async function reconnect() {
    const prevStatus = status;
    setSaving(true);
    setStatus("connecting");
    try {
      await api.imSetSettings(appId, secret);
      setSavedAppId(appId);
      if (secret.length > 0) setHasSecret(true);
      setSecret("");
      void api.imStatus().then(setStatus);
    } catch (err) {
      setStatus(prevStatus);
      throw err;
    } finally {
      setSaving(false);
    }
  }

  const online = status.startsWith("online");
  const connecting = status.startsWith("connecting");
  const errored = status.startsWith("error");
  let dot = "bg-ink-faint";
  let statusTone = "border-border bg-bg text-ink-faint";
  let statusText = t("settings.imOffline");
  if (online) {
    dot = "bg-success";
    statusTone = "border-success/30 bg-success/15 text-success";
    statusText = t("settings.imOnline");
  } else if (connecting) {
    dot = "bg-waiting";
    statusTone = "border-waiting/30 bg-waiting/15 text-waiting";
    statusText = t("settings.imConnecting");
  } else if (errored) {
    dot = "bg-danger";
    statusTone = "border-danger/30 bg-danger/15 text-danger";
    statusText = t("settings.imError");
  }

  let helperText = " ";
  if (loaded && enabled && bound) {
    helperText = t("settings.imBound");
  } else if (loaded && enabled) {
    helperText = t("settings.imUnbound");
  } else if (loaded) {
    helperText = t("settings.imCollapsedHint");
  }

  let reconnectLabel = t("settings.imConnect");
  if (saving) {
    reconnectLabel = t("settings.imConnecting");
  } else if (online) {
    reconnectLabel = t("settings.imReconnect");
  }

  return (
    <div className="flex flex-col gap-10">
      <div className="rounded-[var(--radius-lg)] border border-border bg-surface">
        <div className="flex items-center gap-3 px-4 py-3.5">
          <div className="grid h-8 w-8 shrink-0 place-items-center rounded-[var(--radius-md)] bg-bg text-ink-muted">
            <MessageSquare size={16} />
          </div>
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <span className="text-[13px] font-semibold text-ink">{t("settings.imProvider")}</span>
              {enabled && (
                <span
                  className={cn(
                    "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11px] font-medium tabular-nums",
                    statusTone,
                  )}
                >
                  <span className={cn("h-1.5 w-1.5 rounded-full", dot)} />
                  {statusText}
                </span>
              )}
            </div>
            <p className="mt-0.5 text-[12px] text-ink-faint">{helperText}</p>
          </div>
          {loaded ? (
            <Toggle on={enabled} onChange={(v) => void toggle(v)} label={t("settings.imProvider")} />
          ) : (
            <div
              aria-hidden
              className="h-[22px] w-[38px] shrink-0 rounded-full bg-border-strong/40"
            />
          )}
        </div>

        {enabled && (
          <div className="flex flex-col gap-4 border-t border-border px-4 py-4">
            <div className="flex gap-1 rounded-[var(--radius-md)] bg-bg p-1">
              {(["scan", "manual"] as const).map((m) => (
                <button
                  key={m}
                  type="button"
                  onClick={() => setMode(m)}
                  className={cn(
                    "flex flex-1 items-center justify-center gap-1.5 rounded-[var(--radius-sm)] px-3 py-1.5 text-[12px] font-medium transition-colors",
                    mode === m
                      ? "bg-surface text-ink shadow-sm"
                      : "text-ink-faint hover:text-ink-muted",
                  )}
                >
                  {m === "scan" ? <QrCode size={13} /> : <KeyRound size={13} />}
                  {m === "scan" ? t("settings.imModeScan") : t("settings.imModeManual")}
                </button>
              ))}
            </div>

            {mode === "scan" ? (
              <div className="flex flex-col gap-3">
                <p className="text-[12px] leading-relaxed text-ink-faint">
                  {t("settings.imScanModeHint")}
                </p>
                <Button variant="primary" onClick={() => setScanOpen(true)} className="self-start">
                  <QrCode size={14} />
                  {t("settings.imScanConnect")}
                </Button>
              </div>
            ) : (
              <div className="flex flex-col gap-4">
                <div className="flex items-start justify-between gap-3">
                  <p className="text-[12px] leading-relaxed text-ink-faint">
                    {t("settings.imManualModeHint")}
                  </p>
                  <button
                    type="button"
                    onClick={() => void api.openUrl("https://open.feishu.cn/app")}
                    className="inline-flex shrink-0 items-center gap-1 text-[12px] font-medium text-brand underline decoration-brand/40 underline-offset-2 hover:decoration-brand"
                  >
                    {t("settings.imOpenPlatform")}
                    <ExternalLink size={12} />
                  </button>
                </div>
                <ImField label={t("settings.imAppId")} hint={t("settings.imAppIdHint")}>
                  <Input
                    value={appId}
                    placeholder={t("settings.imAppIdPlaceholder")}
                    onChange={(e) => setAppId(e.currentTarget.value)}
                    className="h-8 w-full bg-bg/80 font-mono text-[12px]"
                  />
                </ImField>
                <ImField label={t("settings.imAppSecret")} hint={t("settings.imAppSecretHint")}>
                  <Input
                    type="password"
                    value={secret}
                    placeholder={hasSecret ? "••••••••" : ""}
                    onChange={(e) => setSecret(e.currentTarget.value)}
                    className="h-8 w-full bg-bg/80 font-mono text-[12px]"
                  />
                </ImField>
                <ImField label={t("settings.imPermsLabel")} hint={t("settings.imPermsHint")}>
                  <div className="flex items-start gap-2">
                    <code className="flex-1 whitespace-pre rounded-[var(--radius-md)] border border-border bg-bg/80 px-2.5 py-2 font-mono text-[11.5px] leading-relaxed text-ink-muted">
                      {FEISHU_SCOPES.join("\n")}
                    </code>
                    <Button
                      variant="default"
                      size="sm"
                      onClick={() => void copyPerms()}
                      className="shrink-0"
                    >
                      {copied ? <Check size={13} /> : <Copy size={13} />}
                      {copied ? t("settings.imPermsCopied") : t("settings.imPermsCopy")}
                    </Button>
                  </div>
                </ImField>
                <div className="flex justify-end">
                  <Button
                    variant="primary"
                    onClick={() => void reconnect()}
                    disabled={saving || !dirty}
                  >
                    {reconnectLabel}
                  </Button>
                </div>
              </div>
            )}
            <FeishuScanDialog
              open={scanOpen}
              onOpenChange={setScanOpen}
              onConnected={() => void api.imStatus().then(setStatus)}
            />
          </div>
        )}
      </div>
      <ImRoutes />
    </div>
  );
}

function ImField({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="flex flex-col gap-1.5">
      <div className="text-[12.5px] font-medium text-ink">{label}</div>
      {children}
      {hint && <p className="text-[11.5px] leading-relaxed text-ink-faint">{hint}</p>}
    </div>
  );
}

/// 扫码接入飞书 dialog:打开即 begin device-flow、渲染二维码、按服务端建议间隔轮询
/// 状态;成功自动关闭并刷新连接状态,过期 / 出错可重新扫码。关闭 / 卸载时取消后台轮询。
function FeishuScanDialog({
  open,
  onOpenChange,
  onConnected,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onConnected: () => void;
}) {
  const { t } = useTranslation();
  const [qr, setQr] = useState<string | null>(null);
  const [phase, setPhase] = useState<"loading" | "waiting" | "success" | "expired" | "error">(
    "loading",
  );
  const [errReason, setErrReason] = useState<string | null>(null);
  // attempt 自增触发 effect 重跑(重新扫码)。onConnected/onOpenChange 是稳定的 setter
  // 闭包,故意不列入依赖——否则父组件每次渲染都会重启 device-flow。
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    let timer: number | null = null;
    const stop = () => {
      if (timer !== null) {
        clearInterval(timer);
        timer = null;
      }
    };
    setPhase("loading");
    setQr(null);
    setErrReason(null);
    void api
      .feishuScanBegin()
      .then((b) => {
        if (!alive) return;
        setQr(b.qr_data_uri);
        setPhase("waiting");
        timer = window.setInterval(() => {
          void api.feishuScanStatus().then((s) => {
            if (!alive) return;
            if (s.status === "success") {
              stop();
              setPhase("success");
              onConnected();
              window.setTimeout(() => {
                if (alive) onOpenChange(false);
              }, 1200);
            } else if (s.status === "expired") {
              stop();
              setPhase("expired");
            } else if (s.status === "error") {
              stop();
              setErrReason(s.error_reason);
              setPhase("error");
            }
          });
        }, b.poll_interval_ms);
      })
      .catch(() => {
        if (alive) setPhase("error");
      });
    return () => {
      alive = false;
      stop();
      void api.feishuScanCancel();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, attempt]);

  const errorText =
    errReason === "lark_unsupported"
      ? t("settings.imScanLarkUnsupported")
      : t("settings.imScanError");

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent title={t("settings.imScanTitle")}>
        <div className="flex flex-col items-center gap-4 py-2">
          {phase === "loading" && (
            <p className="text-[12.5px] text-ink-faint">{t("settings.imScanLoading")}</p>
          )}
          {phase === "waiting" && qr && (
            <>
              <img
                src={qr}
                alt=""
                className="h-48 w-48 rounded-[var(--radius-md)] border border-border bg-white p-2"
              />
              <p className="text-center text-[12.5px] text-ink-muted">
                {t("settings.imScanHint")}
              </p>
            </>
          )}
          {phase === "success" && (
            <p className="text-[13px] font-medium text-success">{t("settings.imScanSuccess")}</p>
          )}
          {(phase === "expired" || phase === "error") && (
            <>
              <p className="text-center text-[12.5px] text-ink-muted">
                {phase === "expired" ? t("settings.imScanExpired") : errorText}
              </p>
              <Button variant="primary" onClick={() => setAttempt((a) => a + 1)}>
                {t("settings.imScanRetry")}
              </Button>
            </>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/** 已绑定的 issue ↔ 飞书话题映射；绑定动作走「在飞书话题里
 *  发 `/bind <thread_id>` 给 bot」的入站协议；Settings 提供查看与解绑。 */
function ImRoutes() {
  const { t } = useTranslation();
  const [rows, setRows] = useState<import("../lib/types").ImRoute[]>([]);
  const [loading, setLoading] = useState(false);

  async function refresh() {
    setLoading(true);
    try {
      setRows(await api.imListRoutes());
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  async function unbind(threadId: number) {
    await api.imUnbindThread(threadId);
    await refresh();
  }

  let content: ReactNode;
  if (loading && rows.length === 0) {
    content = (
      <span className="text-[12px] text-ink-faint">{t("settings.imRoutesLoading")}</span>
    );
  } else if (rows.length === 0) {
    content = <span className="text-[12px] text-ink-faint">{t("settings.imRoutesEmpty")}</span>;
  } else {
    content = rows.map((r) => (
      <div
        key={r.thread_id}
        className="flex items-center justify-between gap-3 rounded-md border border-border bg-bg/40 px-2.5 py-1.5"
      >
        <div className="flex min-w-0 flex-1 flex-col">
          <span className="font-mono text-[11px] text-ink">
            #{r.thread_id} · {r.channel}
          </span>
          <span className="truncate font-mono text-[11px] text-ink-muted">
            {r.chat_id} / {r.im_thread_ref}
          </span>
        </div>
        <Button variant="default" onClick={() => void unbind(r.thread_id)}>
          {t("settings.imRoutesUnbind")}
        </Button>
      </div>
    ));
  }

  return (
    <SettingsGroup title={t("settings.imRoutesGroup")}>
      <div className="flex flex-col gap-2.5 px-3 py-3">
        <div>
          <div className="text-[12.5px] font-semibold text-ink">
            {t("settings.imRoutesLabel")}
          </div>
          <p className="mt-1 max-w-[58ch] text-[12px] leading-relaxed text-ink-faint">
            {t("settings.imRoutesHint")}
          </p>
        </div>
        <div className="flex flex-col gap-1.5">{content}</div>
      </div>
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
