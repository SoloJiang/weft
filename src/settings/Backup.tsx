import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { Button } from "../components/ui/Button";
import { Input } from "../components/ui/Input";
import { Toggle } from "../components/ui/Toggle";
import { api } from "../lib/api";
import { cn } from "../lib/cn";
import type { BackupStatusDto } from "../lib/types";

/** Settings → Backup panel. Mirrors the structure of ImSettings: each group is
 *  a SettingsGroup card with SettingRow rows; controls live in the right column.
 *  All backend calls go through `api.backup*` (lib/api.ts). */
export function BackupSettings() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<BackupStatusDto | null>(null);
  const [enabled, setEnabled] = useState(false);
  const [remoteUrl, setRemoteUrl] = useState("");
  const [autoBackup, setAutoBackup] = useState(true);
  const [onExit, setOnExit] = useState(true);
  const [testState, setTestState] = useState<
    | { kind: "idle" }
    | { kind: "testing" }
    | { kind: "ok" }
    | { kind: "err"; msg: string }
  >({ kind: "idle" });
  const [saving, setSaving] = useState(false);
  const [running, setRunning] = useState(false);
  const [showRecoveryDialog, setShowRecoveryDialog] = useState(false);
  const [, setTick] = useState(0);
  const [restoreOpen, setRestoreOpen] = useState(false);
  const [restoreRemote, setRestoreRemote] = useState("");
  const [restoreKeyPath, setRestoreKeyPath] = useState("");
  const [restoreState, setRestoreState] = useState<
    { kind: "idle" } | { kind: "running" } | { kind: "ok" } | { kind: "err"; msg: string }
  >({ kind: "idle" });

  const reload = async () => {
    const s = await api.backupGetStatus();
    setStatus(s);
    setEnabled(s.enabled);
    setRemoteUrl(s.remoteUrl);
    setAutoBackup(s.autoBackupEnabled);
    setOnExit(s.backupOnExit);
  };

  useEffect(() => {
    void reload();
  }, []);

  // Tick once a minute so the "last backup … ago" relative time keeps moving
  // even while the panel stays open.
  useEffect(() => {
    const id = setInterval(() => setTick((n) => n + 1), 30_000);
    return () => clearInterval(id);
  }, []);

  const testConn = async () => {
    if (!remoteUrl) return;
    setTestState({ kind: "testing" });
    try {
      await api.backupTestRemote(remoteUrl);
      setTestState({ kind: "ok" });
    } catch (e: unknown) {
      setTestState({ kind: "err", msg: String(e) });
    }
  };

  const onSaveClick = async () => {
    // First-time enable forces the user through the Recovery Key download flow —
    // backups without the key file are unrecoverable, so we block save until
    // they've at least had the chance to grab it.
    if (enabled && !status?.enabled) {
      setShowRecoveryDialog(true);
      return;
    }
    await savePrefs();
  };

  const savePrefs = async () => {
    setSaving(true);
    try {
      await api.backupSavePrefs(enabled, remoteUrl, autoBackup, onExit);
      await reload();
    } finally {
      setSaving(false);
    }
  };

  const downloadRecoveryKey = async () => {
    const target = await saveDialog({
      defaultPath: "weft-recovery-key.json",
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (!target) return;
    await api.backupExportRecoveryKey(target);
    setShowRecoveryDialog(false);
    // Only after the user actually saved the key do we persist the enable.
    await savePrefs();
  };

  const runNow = async () => {
    setRunning(true);
    try {
      const s = await api.backupRunNow();
      setStatus(s);
    } catch (e: unknown) {
      setStatus((prev) => (prev ? { ...prev, lastError: String(e) } : prev));
    } finally {
      setRunning(false);
    }
  };

  const pickRestoreKey = async () => {
    const sel = await openDialog({
      directory: false,
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (typeof sel === "string") setRestoreKeyPath(sel);
  };

  const doRestore = async () => {
    if (!restoreKeyPath) {
      setRestoreState({ kind: "err", msg: t("settings.backupRestoreNoKey") });
      return;
    }
    setRestoreState({ kind: "running" });
    try {
      await api.backupRestore(restoreRemote, restoreKeyPath);
      setRestoreState({ kind: "ok" });
    } catch (e: unknown) {
      setRestoreState({ kind: "err", msg: String(e) });
    }
  };

  return (
    <div className="flex flex-col gap-10">
      <SettingsGroup title={t("settings.backupGroupRemote")}>
        <SettingRow label={t("settings.backupEnable")} hint={t("settings.backupEnableHint")}>
          <Toggle on={enabled} onChange={setEnabled} label={t("settings.backupEnable")} />
        </SettingRow>
        <SettingRow label={t("settings.backupRemoteLabel")} hint={t("settings.backupRemoteHint")}>
          <div className="flex w-[360px] max-w-[42vw] flex-col items-end gap-1.5">
            <Input
              value={remoteUrl}
              placeholder={t("settings.backupRemotePlaceholder")}
              onChange={(e) => setRemoteUrl(e.currentTarget.value)}
              className="h-8 w-full bg-bg/80 font-mono text-[12px]"
            />
            <div className="flex w-full items-center justify-end gap-2">
              {testState.kind === "ok" && (
                <span className="text-[11px] text-success">{t("settings.backupTestOk")}</span>
              )}
              {testState.kind === "err" && (
                <span className="truncate text-[11px] text-danger" title={testState.msg}>
                  {testState.msg.split("\n")[0]}
                </span>
              )}
              <Button
                variant="default"
                onClick={() => void testConn()}
                disabled={testState.kind === "testing" || !remoteUrl}
              >
                {testState.kind === "testing"
                  ? t("settings.backupTesting")
                  : t("settings.backupTest")}
              </Button>
            </div>
          </div>
        </SettingRow>
      </SettingsGroup>

      <SettingsGroup title={t("settings.backupGroupSchedule")}>
        <SettingRow label={t("settings.backupAuto")} hint={t("settings.backupAutoHint")}>
          <Toggle on={autoBackup} onChange={setAutoBackup} label={t("settings.backupAuto")} />
        </SettingRow>
        <SettingRow label={t("settings.backupOnExit")} hint={t("settings.backupOnExitHint")}>
          <Toggle on={onExit} onChange={setOnExit} label={t("settings.backupOnExit")} />
        </SettingRow>
        <div className="flex items-center justify-end gap-2 px-3 py-3">
          <Button
            variant="default"
            onClick={() => void runNow()}
            disabled={running || !enabled || !status?.enabled}
          >
            {running ? t("settings.backupRunning") : t("settings.backupRunNow")}
          </Button>
          <Button variant="primary" onClick={() => void onSaveClick()} disabled={saving}>
            {saving ? t("settings.backupSaving") : t("settings.backupSave")}
          </Button>
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.backupGroupRecovery")}>
        <SettingRow
          label={t("settings.backupExportRecoveryKey")}
          hint={t("settings.backupRecoveryKeyHint")}
        >
          <Button variant="default" onClick={() => setShowRecoveryDialog(true)}>
            {t("settings.backupRecoveryKeyDownload")}
          </Button>
        </SettingRow>
        {showRecoveryDialog && (
          <div className="border-t border-border bg-waiting/10 px-3 py-3">
            <p className="text-[12px] leading-relaxed text-ink">
              {t("settings.backupRecoveryKeyDialog")}
            </p>
            <div className="mt-2 flex justify-end gap-2">
              <Button variant="default" onClick={() => setShowRecoveryDialog(false)}>
                {t("settings.backupCancel")}
              </Button>
              <Button variant="primary" onClick={() => void downloadRecoveryKey()}>
                {t("settings.backupRecoveryKeyDownload")}
              </Button>
            </div>
          </div>
        )}
      </SettingsGroup>

      <SettingsGroup title={t("settings.backupGroupStatus")}>
        <div className="flex flex-col gap-2 px-3 py-3 text-[12px] text-ink">
          <StatusLine status={status} />
          {status?.lastError && (
            <div className="text-danger" title={status.lastError}>
              {status.lastError.split("\n")[0]}
            </div>
          )}
        </div>
      </SettingsGroup>

      <SettingsGroup title={t("settings.backupGroupRestore")}>
        <SettingRow label={t("settings.backupRestoreLink")} hint={t("settings.backupRestoreHint")}>
          <Button variant="default" onClick={() => setRestoreOpen((v) => !v)}>
            {restoreOpen ? t("settings.backupCancel") : t("settings.backupRestoreLink")}
          </Button>
        </SettingRow>
        {restoreOpen && (
          <div className="flex flex-col gap-3 border-t border-border px-3 py-3">
            <div className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">
                {t("settings.backupRestoreRemoteLabel")}
              </span>
              <Input
                value={restoreRemote}
                placeholder={t("settings.backupRemotePlaceholder")}
                onChange={(e) => setRestoreRemote(e.currentTarget.value)}
                className="h-8 w-full bg-bg/80 font-mono text-[12px]"
              />
            </div>
            <div className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">
                {t("settings.backupRestoreKeyLabel")}
              </span>
              <div className="flex items-center gap-2">
                <Input
                  value={restoreKeyPath}
                  placeholder="/path/to/weft-recovery-key.json"
                  onChange={(e) => setRestoreKeyPath(e.currentTarget.value)}
                  className="h-8 min-w-0 flex-1 bg-bg/80 font-mono text-[12px]"
                />
                <Button variant="default" onClick={() => void pickRestoreKey()}>
                  {t("settings.backupRestorePickKey")}
                </Button>
              </div>
            </div>
            <div className="flex items-center justify-end gap-2">
              {restoreState.kind === "ok" && (
                <span className="text-[11px] text-success">
                  {t("settings.backupRestoreSuccess")}
                </span>
              )}
              {restoreState.kind === "err" && (
                <span className="truncate text-[11px] text-danger" title={restoreState.msg}>
                  {restoreState.msg.split("\n")[0]}
                </span>
              )}
              <Button
                variant="primary"
                onClick={() => void doRestore()}
                disabled={restoreState.kind === "running" || !restoreRemote}
              >
                {restoreState.kind === "running"
                  ? t("settings.backupRestoring")
                  : t("settings.backupRestoreConfirm")}
              </Button>
            </div>
          </div>
        )}
      </SettingsGroup>
    </div>
  );
}

function StatusLine({ status }: { status: BackupStatusDto | null }) {
  const { t } = useTranslation();
  if (!status?.lastBackupAt) return <span>{t("settings.backupStatusNever")}</span>;
  const last = parseInt(status.lastBackupAt, 10);
  return (
    <div className="flex flex-col gap-1">
      <div>
        {t("settings.backupStatusAgo")}: {relTime(last)}
        {status.lastBackupCommitSha && (
          <span className="font-mono text-ink-faint">
            {" · "}
            {status.lastBackupCommitSha.slice(0, 8)}
            {status.lastBackupBytes !== null && ` · ${fmtBytes(status.lastBackupBytes)}`}
          </span>
        )}
      </div>
      {status.enabled && status.autoBackupEnabled && (
        <div className="text-ink-muted">
          {t("settings.backupNextAt")}: {relTime(last + status.intervalSeconds)}
        </div>
      )}
    </div>
  );
}

function relTime(unixSecs: number): string {
  if (!Number.isFinite(unixSecs)) return "—";
  const diff = Math.floor(Date.now() / 1000) - unixSecs;
  const sign = diff >= 0 ? "ago" : "in";
  const d = Math.abs(diff);
  if (d < 60) return `${d}s ${sign}`;
  if (d < 3600) return `${Math.floor(d / 60)}m ${sign}`;
  if (d < 86400) return `${Math.floor(d / 3600)}h ${sign}`;
  return `${Math.floor(d / 86400)}d ${sign}`;
}

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

// Local copies of the two layout helpers from SettingsDialog. We don't export
// them from that file (and one of them is a primitive any settings panel may
// want to lay out differently). Keeping a local pair avoids a cross-cutting
// refactor for one new panel.
function SettingsGroup({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-3">
      <h2 className="text-[13px] font-semibold text-ink">{title}</h2>
      <div
        className={cn(
          "flex flex-col rounded-[var(--radius-lg)] border border-border bg-surface",
          "[&>div+div]:border-t [&>div+div]:border-border",
        )}
      >
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
