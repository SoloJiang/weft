/* Shared shell + atoms. Uses window globals from icons.jsx / data.jsx. */

const LANES = {
  queued:    { label: "队列",   en: "Queued",      cls: "st-idle" },
  working:   { label: "进行中", en: "In progress", cls: "st-running" },
  needs:     { label: "待你处理", en: "Needs you",   cls: "st-waiting" },
  review:    { label: "评审中", en: "In review",   cls: "st-inject" },
  delivered: { label: "已交付", en: "Delivered",   cls: "st-delivered" },
};
const LANE_ORDER = ["queued", "working", "needs", "review", "delivered"];

/* i18n layer-1 (UI chrome). Content / agent output is layer-2 (its own language). */
function L(zh, en) { return window.__lang === "EN" ? en : zh; }

/* tool glyph + optional name */
function Tool({ id, withName }) {
  const t = window.TOOLS[id];
  if (!t) return null;
  return (
    <span className="tool">
      <span className={"glyph " + t.cls}>{t.glyph}</span>
      {withName && <span>{t.name}</span>}
    </span>
  );
}

function LaneTag({ lane, dot }) {
  const l = LANES[lane];
  return (
    <span className={"st " + l.cls}>
      {dot !== false && <span className="dot" />}
      {window.__lang === "EN" ? l.en : l.label}
    </span>
  );
}

/* scope role chip: write / read / none */
function ScopeRole({ role, small }) {
  const map = {
    write: { Icon: IconPencil, label: "写", color: "var(--weft)", bg: "var(--weft-ghost)" },
    read:  { Icon: IconEye,    label: "只读", color: "var(--warp)", bg: "var(--warp-ghost)" },
    none:  { Icon: IconBan,    label: "不涉及", color: "var(--ink-faint)", bg: "transparent" },
  };
  const m = map[role];
  return (
    <span className="chip" style={{ color: m.color, background: m.bg, borderColor: role === "none" ? "var(--border)" : "transparent",
      height: small ? 19 : 21, fontSize: small ? 10.5 : 11 }}>
      <m.Icon size={12} /> {m.label}
    </span>
  );
}

/* acceptance trust signals */
function Signal({ kind, label }) {
  const map = {
    pass: { cls: "pass", Icon: IconCheck },
    fail: { cls: "fail", Icon: IconX },
    pend: { cls: "pend", Icon: IconClock },
    warn: { cls: "warn", Icon: IconWarn },
  };
  const m = map[kind] || map.pend;
  return <span className={"signal " + m.cls}><m.Icon /> {label}</span>;
}
function Signals({ s }) {
  const testKind = s.tests[0] === s.tests[1] ? "pass" : (s.tests[0] === 0 ? "pend" : "warn");
  return (
    <div className="row gap3" style={{ flexWrap: "wrap" }}>
      <Signal kind={testKind} label={`tests ${s.tests[0]}/${s.tests[1]}`} />
      <Signal kind={s.type} label="types" />
      <Signal kind={s.contract} label="契约" />
      <Signal kind={s.review} label={s.review === "pass" ? "review ✓" : "review"} />
    </div>
  );
}

/* ----------------------------- NEEDS-YOU DOCK ----------------------------- */
/* Strategically the most prominent element: pinned under the top bar on every
   screen, aggregating exceptions across all threads. Empty = calm all-clear. */
function NeedsDock({ needs, expanded, onToggle, onResolve, onGoto, onToast }) {
  const n = needs.length;
  if (n === 0) {
    return (
      <div className="needs-dock clear">
        <span className="st st-running"><span className="dot" /></span>
        <span className="t-label mut">{L("自动流转中 · 暂无待你处理的事项", "Flowing automatically · nothing needs you")}</span>
        <span className="grow" />
        <span className="t-meta">{L("5 个 issue 自动推进中", "5 issues in flight")}</span>
      </div>
    );
  }
  const KIND = {
    permission: { Icon: IconShieldQ, label: "工具权限", color: "var(--st-waiting)" },
    escalation: { Icon: IconBolt,    label: "Agent 升级", color: "var(--st-inject)" },
    conflict:   { Icon: IconWarn,    label: "硬冲突",     color: "var(--st-error)" },
  };
  const top = needs[0];
  const k0 = KIND[top.kind];
  return (
    <div className={"needs-dock active" + (expanded ? " open" : "")}>
      <button className="needs-head" onClick={onToggle}>
        <span className="needs-pip" style={{ background: "var(--st-waiting)" }}>{n}</span>
        <span className="t-h3" style={{ color: "var(--st-waiting)" }}>{L("待你处理", "Needs you")}</span>
        {!expanded && (
          <span className="needs-peek mut truncate">
            <k0.Icon size={13} style={{ color: k0.color, flex: "0 0 auto" }} />
            <b style={{ color: "var(--ink)", fontWeight: 600 }}>{top.title}</b>
            <span className="faint">· {top.thread}</span>
          </span>
        )}
        <span className="grow" />
        <span className="t-meta">{L(n + " 项异常 · 聚合自全部 issue", n + " exceptions · across all issues")}</span>
        <span className={"needs-chev" + (expanded ? " up" : "")}><IconChevD size={15} /></span>
      </button>
      {expanded && (
        <div className="needs-list fade-in">
          {needs.map((it) => {
            const k = KIND[it.kind];
            return (
              <div key={it.id} className="needs-item">
                <span className="needs-ico" style={{ color: k.color }}>
                  <k.Icon size={15} />
                </span>
                <div className="grow col" style={{ gap: 3 }}>
                  <div className="row gap2">
                    <span className="t-label" style={{ fontWeight: 600 }}>{it.title}</span>
                    <span className="chip" style={{ height: 18, fontSize: 10, color: k.color, borderColor: "var(--border)" }}>{k.label}</span>
                  </div>
                  <div className="t-meta">
                    {it.tool !== "—" && <span style={{ marginRight: 6 }}><Tool id={it.tool} /></span>}
                    {it.thread} · {it.direction} · {it.age} 前
                  </div>
                  {it.detail && <div className="mono needs-detail">{it.detail}</div>}
                  {it.reason && <div className="t-meta">{it.reason}</div>}
                </div>
                <div className="row gap1 needs-acts">
                  {it.kind === "permission" && <>
                    <button className="btn btn-sm btn-primary" onClick={() => { onResolve(it.id); onToast && onToast("success", "已允许 · Codex 继续执行"); }}><IconCheck size={13} /> 允许</button>
                    <button className="btn btn-sm btn-default" onClick={() => { onResolve(it.id); onToast && onToast("success", "已记住:本 issue 始终允许该命令"); }}>始终</button>
                    <button className="btn btn-sm btn-danger"><IconX size={13} /></button>
                  </>}
                  {it.kind === "escalation" && <>
                    <button className="btn btn-sm btn-primary" onClick={() => onGoto && onGoto("home")}>回复</button>
                    <button className="btn btn-sm btn-default" onClick={() => onResolve(it.id)}>略过</button>
                  </>}
                  {it.kind === "conflict" && <>
                    <button className="btn btn-sm btn-default" onClick={() => onGoto && onGoto("session")}><IconTerminal size={13} /> 打开</button>
                    <button className="btn btn-sm btn-default"><IconMerge size={13} /> rebase</button>
                  </>}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

/* segmented control (tabs) */
function Segmented({ options, value, onChange }) {
  return (
    <div className="seg">
      {options.map((o) => (
        <button key={o.id} className={"seg-btn" + (o.id === value ? " on" : "")} onClick={() => onChange(o.id)}>
          {o.Icon && <o.Icon size={14} />} {o.label}
        </button>
      ))}
    </div>
  );
}

/* ------------------------------- LEFT RAIL ------------------------------- */
function LeftRail({ screen, onNav, onDialog }) {
  const nav = [
    { id: "home", label: L("控制台", "Console"), Icon: IconHome },
    { id: "board", label: L("看板", "Board"), Icon: IconBoard },
    { id: "repos", label: L("仓库地图", "Repo map"), Icon: IconRepos },
  ];
  return (
    <aside className="rail">
      <div className="rail-top">
        <button className="ws-switch" onClick={() => onNav("onboard")} title="新建工作区 · 首用流">
          <WeaveMark size={20} />
          <div className="col" style={{ gap: 1, lineHeight: 1.1 }}>
            <span className="t-label" style={{ fontWeight: 600 }}>结算改版</span>
            <span className="t-meta">{L("6 仓 · 5 个 issue", "6 repos · 5 issues")}</span>
          </div>
          <IconChevD size={14} className="faint" />
        </button>
      </div>

      <div className="rail-sec">
        {nav.map((n) => (
          <button key={n.id} className={"rail-item" + (screen === n.id ? " on" : "")} onClick={() => onNav(n.id)}>
            {screen === n.id && <span className="rail-mark" />}
            <n.Icon size={16} /> <span>{n.label}</span>
          </button>
        ))}
      </div>

      <div className="rail-label row">
        <span className="t-eyebrow">issues</span>
        <span className="grow" />
        <button className="btn-icon sm" style={{ width: 22, height: 22 }} title="新建 issue" onClick={() => onDialog("new-issue")}><IconPlus size={14} /></button>
      </div>
      <div className="rail-threads scroll-y">
        {window.THREADS.map((t) => (
          <button key={t.id} className={"thread-item" + (t.id === "t-discount" && screen === "thread" ? " on" : "")}
                  onClick={() => onNav("thread", t.id)}>
            <span className={"st " + LANES[t.lane].cls}><span className="dot" /></span>
            <span className="grow truncate">{t.title}</span>
            {t.needs > 0 && <span className="thread-need">{t.needs}</span>}
            <span className="t-meta tnum">{t.progress.done}/{t.progress.total}</span>
          </button>
        ))}
      </div>

      <button className="rail-foot" onClick={() => onNav("settings")}>
        <IconSettings size={15} /> <span className="grow" style={{ textAlign: "left" }}>设置</span>
        <span className="chip rail-foot-link" style={{ height: 17, fontSize: 9.5 }}
              onClick={(e) => { e.stopPropagation(); onNav("notes"); }}>设计提案</span>
      </button>
    </aside>
  );
}

/* ------------------------------- TOP BAR ------------------------------- */
function TopBar({ theme, onTheme, lang, onLang, onPalette, crumbs, onToggleRail }) {
  return (
    <header className="topbar">
      <button className="btn-icon" onClick={onToggleRail} title="收起 / 展开侧栏"><IconSidebar size={16} /></button>
      <div className="row gap2 grow" style={{ minWidth: 0 }}>
        {crumbs}
      </div>
      <button className="cmdk" onClick={onPalette}>
        <IconSearch size={14} /> <span className="mut">{L("搜索 · 跳转 · 动作", "Search · Go · Act")}</span> <span className="kbd">⌘K</span>
      </button>
      <button className="btn-icon" onClick={onLang} title="语言"><span className="t-label" style={{ fontWeight: 600 }}>{lang}</span></button>
      <button className="btn-icon" onClick={onTheme} title="主题">{theme === "dark" ? <IconSun size={16} /> : <IconMoon size={16} />}</button>
    </header>
  );
}

Object.assign(window, {
  LANES, LANE_ORDER, Tool, LaneTag, ScopeRole, Signal, Signals,
  NeedsDock, Segmented, LeftRail, TopBar,
});
