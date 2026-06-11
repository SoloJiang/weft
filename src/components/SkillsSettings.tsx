import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Boxes, RefreshCw, Trash2, Plus } from "lucide-react";
import { useStore } from "../state/store";
import { api } from "../lib/api";
import type { EnabledSkill, ParsedSkill, SkillSource } from "../lib/types";
import { Button } from "./ui/Button";
import { Input } from "./ui/Input";
import { Toggle } from "./ui/Toggle";
import { cn } from "../lib/cn";

export function SkillsSettings() {
  const { t } = useTranslation();
  const { activeWorkspaceId, markSkillsDirty } = useStore();
  const [sources, setSources] = useState<SkillSource[]>([]);
  const [url, setUrl] = useState("");
  const [ref, setRef] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = () => void api.listSkillSources().then(setSources).catch(() => {});
  useEffect(() => {
    refresh();
  }, []);

  async function add() {
    if (!url.trim() || busy) return;
    setBusy(true);
    try {
      await api.addSkillSource(url.trim(), ref.trim() || undefined);
      setUrl("");
      setRef("");
      markSkillsDirty();
      refresh();
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-col gap-1.5">
        <p className="text-[13px] text-ink-muted">{t("settings.skillsHint")}</p>
      </div>
      <div className="flex items-center gap-2">
        <Input
          value={url}
          placeholder={t("settings.skillsGitUrlPlaceholder")}
          onChange={(e) => setUrl(e.currentTarget.value)}
          className="h-8 flex-1 bg-bg/80 font-mono text-[12px]"
        />
        <Input
          value={ref}
          placeholder={t("settings.skillsRefPlaceholder")}
          onChange={(e) => setRef(e.currentTarget.value)}
          className="h-8 w-44 bg-bg/80 font-mono text-[12px]"
        />
        <Button variant="primary" disabled={!url.trim() || busy} onClick={() => void add()}>
          <Plus size={13} />
          {t("settings.skillsAddSource")}
        </Button>
      </div>
      <div className="flex flex-col gap-3">
        {sources.map((s) => (
          <SourceRow key={s.id} source={s} wsId={activeWorkspaceId} onChanged={refresh} />
        ))}
      </div>
    </div>
  );
}

function SourceRow({
  source,
  wsId,
  onChanged,
}: {
  source: SkillSource;
  wsId: number | null;
  onChanged: () => void;
}) {
  const { t } = useTranslation();
  const { markSkillsDirty } = useStore();
  const [skills, setSkills] = useState<ParsedSkill[]>([]);
  const [enabled, setEnabled] = useState<EnabledSkill[]>([]);
  const [syncing, setSyncing] = useState(false);

  const load = () => {
    void api.listParsedSkills(source.id).then(setSkills).catch(() => {});
    if (wsId != null) void api.workspaceSkills(wsId).then(setEnabled).catch(() => {});
  };
  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source.id, source.last_synced, wsId]);

  const errored = source.last_status.startsWith("error");

  async function sync() {
    setSyncing(true);
    try {
      await api.syncSkillSource(source.id);
      markSkillsDirty();
      onChanged();
    } finally {
      setSyncing(false);
    }
  }

  return (
    <div className="rounded-[var(--radius-lg)] border border-border bg-surface">
      <div className="flex items-center gap-2 px-3 py-2.5">
        <Boxes size={14} className="shrink-0 text-ink-faint" />
        <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-ink">{source.git_url}</span>
        <span className={cn("text-[11px]", errored ? "text-danger" : "text-ink-faint")}>
          {syncing
            ? t("settings.skillsSyncing")
            : errored
              ? source.last_status
              : source.last_synced
                ? source.last_status
                : t("settings.skillsNever")}
        </span>
        <button
          type="button"
          title={t("settings.skillsSync")}
          onClick={() => void sync()}
          className="grid h-7 w-7 place-items-center rounded-[var(--radius-sm)] text-ink-faint hover:bg-hover hover:text-ink"
        >
          <RefreshCw size={13} className={syncing ? "animate-spin" : ""} />
        </button>
        <button
          type="button"
          title={t("settings.skillsRemove")}
          onClick={() =>
            void api.removeSkillSource(source.id).then(() => {
              markSkillsDirty();
              onChanged();
            })
          }
          className="grid h-7 w-7 place-items-center rounded-[var(--radius-sm)] text-ink-faint hover:bg-hover hover:text-danger"
        >
          <Trash2 size={13} />
        </button>
      </div>
      {skills.length > 0 ? (
        <div className="flex flex-col divide-y divide-border border-t border-border">
          {skills.map((sk) => (
            <SkillRow key={sk.name} sourceId={source.id} skill={sk} wsId={wsId} onChanged={load} enabled={enabled} />
          ))}
        </div>
      ) : (
        <div className="border-t border-border px-3 py-2 text-[11px] text-ink-faint">
          {t("settings.skillsNoSkills")}
        </div>
      )}
    </div>
  );
}

function SkillRow({
  sourceId,
  skill,
  wsId,
  onChanged,
  enabled,
}: {
  sourceId: number;
  skill: ParsedSkill;
  wsId: number | null;
  onChanged: () => void;
  enabled: EnabledSkill[];
}) {
  const { t } = useTranslation();
  const { markSkillsDirty } = useStore();
  const mine = enabled.find((e) => e.source_id === sourceId && e.name === skill.name);
  const overriddenBy = enabled.find((e) => e.name === skill.name && !e.overridden && e.source_id !== sourceId);
  const [global, setGlobal] = useState(false);
  const [thisWs, setThisWs] = useState(false);

  useEffect(() => {
    setGlobal(false);
    setThisWs(!!mine && !mine.overridden);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skill.name, sourceId]);

  async function toggleGlobal(on: boolean) {
    setGlobal(on);
    await api.setSkillEnabled(sourceId, skill.name, "global", on);
    markSkillsDirty();
    onChanged();
  }
  async function toggleWs(on: boolean) {
    if (wsId == null) return;
    setThisWs(on);
    await api.setSkillEnabled(sourceId, skill.name, `ws:${wsId}`, on);
    markSkillsDirty();
    onChanged();
  }

  return (
    <div className="flex items-center gap-3 px-3 py-2">
      <div className="min-w-0 flex-1">
        <div className="truncate text-[12.5px] font-medium text-ink">{skill.name}</div>
        {skill.description && (
          <div className="truncate text-[11px] text-ink-faint">{skill.description}</div>
        )}
        {overriddenBy && (
          <div className="text-[10.5px] text-waiting">
            {t("settings.skillsOverridden", { source: overriddenBy.source_id })}
          </div>
        )}
      </div>
      <div className="flex items-center gap-3">
        <label className="flex items-center gap-1.5 text-[11px] text-ink-muted">
          {t("settings.skillsGlobal")}
          <Toggle on={global} onChange={(v) => void toggleGlobal(v)} label={t("settings.skillsGlobal")} />
        </label>
        <label className="flex items-center gap-1.5 text-[11px] text-ink-muted">
          {global ? t("settings.skillsGlobalOn") : t("settings.skillsThisWs")}
          <Toggle
            on={global ? true : thisWs}
            onChange={(v) => void toggleWs(v)}
            label={t("settings.skillsThisWs")}
          />
        </label>
      </div>
    </div>
  );
}
