/* App orchestrator: screen routing, theme/lang, the pinned Needs dock,
   command palette. Owns all shared state; screens are presentational. */

const CRUMBS = {
  home:    [["结算改版"], ["控制台"]],
  scope:   [["结算改版"], ["结算加优惠码"], ["Scope"]],
  board:   [["Issue 总览"]],
  thread:  [["结算改版"], ["看板"]],
  session: [["结算加优惠码"], ["优惠码契约 + 校验端点"]],
  repos:   [["仓库地图"]],
  settings:[["设置"]],
  states:  [["状态与边界"]],
  notes:   [["设计提案"]],
};

function Crumbs({ screen, boardLevel }) {
  const key = screen === "board" ? (boardLevel === "thread" ? "thread" : "board") : screen;
  const parts = CRUMBS[key] || CRUMBS.home;
  return (
    <div className="crumb">
      <WeaveMark size={18} />
      {parts.map((p, i) => (
        <React.Fragment key={i}>
          {i > 0 && <span className="sep">/</span>}
          <span className={i === parts.length - 1 ? "crumb-title" : "mut t-label"}>
            {p[0]}{p[1] && <span className="faint" style={{ fontWeight: 400 }}>　{p[1]}</span>}
          </span>
        </React.Fragment>
      ))}
    </div>
  );
}

function Palette({ onClose, onNav, onTheme, onDialog }) {
  const items = [
    { label: "新建 issue…", Icon: IconPlus, go: () => onDialog("new-issue") },
    { label: "添加仓库…", Icon: IconRepos, go: () => onDialog("add-repo") },
    { label: "首用流 · onboarding 演示", Icon: IconFlow, go: () => onNav("onboard") },
    { label: "控制台（Lead 对话为家）", Icon: IconHome, go: () => onNav("home") },
    { label: "Scope 拆解 — 核心 wow", Icon: IconSpark, go: () => onNav("scope") },
    { label: "看板 · Issue 总览", Icon: IconBoard, go: () => onNav("board") },
    { label: "看板 · 子任务（结算加优惠码）", Icon: IconLayers, go: () => onNav("thread", "t-discount") },
    { label: "会话工作台（chat timeline + diff）", Icon: IconTerminal, go: () => onNav("session") },
    { label: "仓库地图 · Curator", Icon: IconRepos, go: () => onNav("repos") },
    { label: "设置 · 含有效配置预览", Icon: IconSettings, go: () => onNav("settings") },
    { label: "状态与边界 · 状态规格", Icon: IconLayers, go: () => onNav("states") },
    { label: "设计提案 · 现状→终态", Icon: IconFlow, go: () => onNav("notes") },
    { label: "删除当前 issue…", Icon: IconWarn, go: () => onDialog("delete-issue") },
    { label: "切换主题 明 / 暗", Icon: IconSun, go: onTheme },
  ];
  return (
    <div className="overlay" onClick={onClose}>
      <div className="palette fade-in" onClick={(e) => e.stopPropagation()}>
        <div className="palette-head"><IconSearch size={16} className="faint" /><input autoFocus className="palette-input" placeholder="跳转 · 动作 · 搜索…" /><span className="kbd">esc</span></div>
        <div className="palette-list">
          {items.map((it, i) => (
            <button key={i} className="palette-item" onClick={() => { it.go(); onClose(); }}>
              <it.Icon size={15} className="faint" /><span className="grow">{it.label}</span><IconArrow size={14} className="faint" />
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

/* error boundary — a screen crash shows a readable fallback + escape hatch,
   never a blank app (失败可读 + 逃生舱). Resets when the screen key changes. */
class ErrorBoundary extends React.Component {
  constructor(p) { super(p); this.state = { err: null }; }
  static getDerivedStateFromError(e) { return { err: e }; }
  render() {
    if (!this.state.err) return this.props.children;
    return (
      <div className="crash">
        <div className="crash-card">
          <span className="dialog-ico t-danger"><IconWarn size={18} /></span>
          <div className="t-h3" style={{ marginTop: 11 }}>这个界面崩溃了</div>
          <div className="t-meta" style={{ marginTop: 4 }}>其余界面与后台 agent 不受影响。已就地递上逃生舱。</div>
          <div className="crash-msg mono">{String((this.state.err && this.state.err.message) || this.state.err)}</div>
          <div className="row gap2" style={{ marginTop: 13 }}>
            <button className="btn btn-default btn-sm" onClick={() => this.setState({ err: null })}><IconReplay size={13} /> 重试</button>
            <button className="btn btn-default btn-sm"><IconCopy size={13} /> 复制错误</button>
            <button className="btn btn-default btn-sm"><IconTerminal size={13} /> 在终端打开</button>
          </div>
        </div>
      </div>
    );
  }
}

function App() {
  const [screen, setScreen] = React.useState("home");
  const [boardLevel, setBoardLevel] = React.useState("workspace");
  const [theme, setTheme] = React.useState(() => document.documentElement.getAttribute("data-theme") || "dark");
  const [lang, setLang] = React.useState("中");
  const [needs, setNeeds] = React.useState(window.NEEDS);
  const [needsOpen, setNeedsOpen] = React.useState(false);
  const [palette, setPalette] = React.useState(false);
  const [railCollapsed, setRailCollapsed] = React.useState(false);
  const [dialog, setDialog] = React.useState(null);
  const [toasts, setToasts] = React.useState([]);
  const toastSeq = React.useRef(0);
  const pushToast = (kind, msg, action) => setToasts((t) => [...t, { id: ++toastSeq.current, kind, msg, action }]);
  const dismissToast = (id) => setToasts((t) => t.filter((x) => x.id !== id));

  React.useEffect(() => { document.documentElement.setAttribute("data-theme", theme); }, [theme]);
  const toggleTheme = () => setTheme((t) => (t === "dark" ? "light" : "dark"));
  window.__lang = lang; // i18n layer-1 source for L(zh,en); content stays layer-2

  React.useEffect(() => {
    const onKey = (e) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") { e.preventDefault(); setPalette((p) => !p); }
      if (e.key === "Escape") { setPalette(false); setNeedsOpen(false); setDialog(null); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const nav = (s, arg) => {
    if (s === "thread") { setScreen("board"); setBoardLevel("thread"); }
    else if (s === "board") { setScreen("board"); setBoardLevel("workspace"); }
    else if (s === "needs") { setNeedsOpen(true); }
    else setScreen(s);
  };
  const goSession = () => setScreen("session");
  const resolveNeed = (id) => setNeeds((n) => n.filter((x) => x.id !== id));

  if (screen === "onboard") return <OnboardScreen onEnter={() => setScreen("home")} />;

  let body;
  if (screen === "home") body = <HomeScreen onNav={nav} onSession={goSession} />;
  else if (screen === "scope") body = <ScopeScreen onNav={nav} />;
  else if (screen === "board") body = <BoardScreen level={boardLevel} onLevel={setBoardLevel} onOpenThread={() => nav("thread", "t-discount")} onSession={goSession} />;
  else if (screen === "session") body = <SessionScreen onNav={nav} onDialog={setDialog} />;
  else if (screen === "repos") body = <ReposScreen onDialog={setDialog} />;
  else if (screen === "settings") body = <SettingsScreen theme={theme} setTheme={setTheme} />;
  else if (screen === "states") body = <StatesScreen />;
  else if (screen === "notes") body = <NotesScreen />;

  return (
    <div className={"app" + (railCollapsed ? " rail-off" : "")}>
      <LeftRail screen={screen === "board" && boardLevel === "thread" ? "thread" : screen} onNav={nav} onDialog={setDialog} />
      <div className="workpane">
        <TopBar theme={theme} onTheme={toggleTheme} lang={lang} onLang={() => setLang((l) => (l === "中" ? "EN" : "中"))}
                onPalette={() => setPalette(true)} onToggleRail={() => setRailCollapsed((c) => !c)} crumbs={<Crumbs screen={screen} boardLevel={boardLevel} />} />
        {/* 待你处理 dock only on work surfaces (supervising delivery); noise on meta/config screens */}
        {["home", "scope", "board", "session"].includes(screen) &&
          <NeedsDock needs={needs} expanded={needsOpen} onToggle={() => setNeedsOpen((o) => !o)} onResolve={resolveNeed} onGoto={nav} onToast={pushToast} />}
        <div className="screen-fade" key={screen === "board" ? "b-" + boardLevel : screen}><ErrorBoundary>{body}</ErrorBoundary></div>
      </div>
      {palette && <Palette onClose={() => setPalette(false)} onNav={nav} onTheme={toggleTheme} onDialog={setDialog} />}
      <Dialogs open={dialog} onClose={() => setDialog(null)} onNav={nav} onToast={pushToast} />
      <Toasts toasts={toasts} onDismiss={dismissToast} />
    </div>
  );
}

ReactDOM.createRoot(document.getElementById("root")).render(<App />);
