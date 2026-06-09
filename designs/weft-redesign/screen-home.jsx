/* HOME = the Lead control tower. A conversation, not a terminal grid.
   Task in → structured cards out (classify / scope / dispatch / contract /
   escalate). Flanked by a live context rail (this thread's trust mini-board,
   the bus, active sessions). */

function ScopeMini({ onOpen }) {
  return (
    <button className="scopemini tile" onClick={onOpen}>
      <div className="scopemini-rows">
        {window.SCOPE.inferred.map((s) => (
          <div key={s.repo} className={"scopemini-row r-" + s.role}>
            <span className="sm-track" />
            <span className="mono sm-repo">{window.repo(s.repo).name}</span>
            <ScopeRole role={s.role} small />
            {s.tool && <Tool id={s.tool} />}
          </div>
        ))}
      </div>
      <div className="row gap2 scopemini-foot">
        <IconSpark size={13} className="warp" />
        <span className="t-meta">3 写 · 2 只读 · 1 不涉及 — 自动推断</span>
        <span className="grow" />
        <span className="warp t-label" style={{ fontWeight: 600 }}>审阅 scope <IconArrow size={13} style={{ verticalAlign: "-2px" }} /></span>
      </div>
    </button>
  );
}

function DispatchCard({ onSession }) {
  return (
    <div className="lead-card">
      <div className="row gap2" style={{ marginBottom: 9 }}>
        <IconFlow size={15} className="warp" />
        <span className="t-h3">已派发 3 个子任务</span>
        <span className="grow" />
        <span className="t-meta">契约先行 · api 先跑</span>
      </div>
      <div className="col" style={{ gap: 6 }}>
        {window.DIRECTIONS.map((d) => (
          <button key={d.id} className="disp-row tile" onClick={() => onSession(d.id)}>
            <span className="disp-order tnum">{d.deps.length ? "2" : "1"}</span>
            <div className="grow col" style={{ gap: 3, minWidth: 0 }}>
              <div className="row gap2"><span className="t-label truncate" style={{ fontWeight: 600 }}>{d.name}</span></div>
              <div className="row gap2"><Tool id={d.tool} withName /><span className="faint">·</span><span className="mono t-meta">{d.write.map((w)=>window.repo(w).name).join(", ")}</span></div>
            </div>
            <LaneTag lane={d.lane} />
            <IconChevR size={15} className="faint" />
          </button>
        ))}
      </div>
    </div>
  );
}

function HomeScreen({ onNav, onSession }) {
  const [draft, setDraft] = React.useState("");
  const endRef = React.useRef(null);

  const renderMsg = (m, i) => {
    if (m.role === "user") {
      return <div key={i} className="msg user fade-in"><div className="bubble">{m.text}</div></div>;
    }
    // lead messages
    let body = null;
    if (m.kind === "classify" || m.kind === "contract") {
      body = <div className="bubble lead" dangerouslySetInnerHTML={{ __html: mdInline(m.text) }} />;
    } else if (m.kind === "scope") {
      body = (
        <div className="bubble lead">
          <div className="row gap2" style={{ marginBottom: 8 }}>
            <IconSpark size={14} className="warp" /><span className="t-label" style={{ fontWeight: 600 }}>{m.title}</span>
          </div>
          <ScopeMini onOpen={() => onNav("scope")} />
        </div>
      );
    } else if (m.kind === "dispatch") {
      body = <DispatchCard onSession={onSession} />;
    } else if (m.kind === "escalate") {
      body = (
        <button className="bubble lead escalate-card tile" onClick={() => onNav("needs")}>
          <span className="needs-ico" style={{ color: "var(--st-waiting)" }}><IconBell size={15} /></span>
          <div className="grow col" style={{ gap: 2 }}>
            <span className="t-label" style={{ fontWeight: 600 }}>1 件待你处理 · Codex 请求执行命令</span>
            <span className="t-meta">其余 3 个子任务继续自动推进，不用等。</span>
          </div>
          <span className="warp t-label nowrap" style={{ fontWeight: 600 }}>处理 <IconArrow size={13} style={{ verticalAlign: "-2px" }} /></span>
        </button>
      );
    }
    return (
      <div key={i} className="msg lead-msg fade-in">
        <span className="lead-ava"><WeaveMark size={16} /></span>
        <div className="grow" style={{ minWidth: 0 }}>{body}</div>
      </div>
    );
  };

  return (
    <div className="ctl">
      <div className="ctl-main">
        <div className="stream scroll-y">
          <div className="stream-inner">
            <div className="ctl-hello">
              <span className="t-eyebrow">控制台 · LEAD（Claude Code）</span>
              <h2 className="t-h2" style={{ marginTop: 4 }}>结算加优惠码</h2>
              <p className="t-meta" style={{ marginTop: 2 }}>只读查看全部 6 个仓 · 不写代码 · 负责规划、划定 scope、驱动 worker 直到交付</p>
            </div>
            {window.LEAD_STREAM.map(renderMsg)}
            <div ref={endRef} />
          </div>
        </div>
        <div className="composer">
          <div className="composer-box">
            <textarea className="composer-input" rows={1} placeholder="给这个 issue 下达任务、回答问题，或提出新需求…"
                      value={draft} onChange={(e) => setDraft(e.target.value)} />
            <div className="composer-foot">
              <button className="btn btn-ghost btn-sm"><IconPlus size={14} /> @ 文件</button>
              <span className="grow" />
              <span className="t-meta">Lead 自动拆解并派发 · Weft 不设审批关卡</span>
              <button className="btn btn-primary btn-sm"><IconSend size={13} /> 发送 <span className="kbd" style={{ marginLeft: 2 }}>⌘↵</span></button>
            </div>
          </div>
        </div>
      </div>

      <aside className="ctl-ctx scroll-y">
        <div className="ctx-sec">
          <div className="ctx-title"><IconLayers size={14} /> 本 issue 进度 <span className="grow" /><button className="btn-ghost t-meta" onClick={() => onNav("thread", "t-discount")}>看板 →</button></div>
          <div className="ctx-prog-bar"><span style={{ width: "33%" }} /></div>
          <div className="row gap3 t-meta" style={{ marginTop: 9, flexWrap: "wrap" }}>
            <span><b style={{ color: "var(--ink)" }}>1</b>/3 子任务</span>
            <span className="st st-running"><span className="dot" />1 进行</span>
            <span className="st st-waiting"><span className="dot" />1 待你</span>
            <span className="st st-inject"><span className="dot" />1 评审</span>
          </div>
        </div>

        <div className="ctx-sec">
          <div className="ctx-title"><IconRadio size={14} /> Agent 协作 <span className="faint t-meta">跨子任务消息</span></div>
          <div className="bus-mini">
            {window.BUS.map((b, i) => (
              <div key={i} className="bus-row">
                <span className={"bus-kind k-" + b.kind} />
                <div className="grow" style={{ minWidth: 0 }}>
                  <div className="row gap1 t-meta"><b className="mono" style={{ color: "var(--ink-muted)" }}>{b.from}</b><IconArrow size={11} /><span className="mono">{b.to}</span><span className="grow" /><span>{b.age}</span></div>
                  <div className="t-label truncate" style={{ color: "var(--ink-muted)", fontWeight: 400 }}>{b.text}</div>
                </div>
              </div>
            ))}
          </div>
        </div>
      </aside>
    </div>
  );
}

/* tiny inline markdown (bold only) */
function mdInline(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;")
    .replace(/\*\*(.+?)\*\*/g, '<b style="font-weight:600">$1</b>')
    .replace(/`(.+?)`/g, '<code class="mono" style="font-size:12px;background:var(--sunken);border:1px solid var(--border);border-radius:4px;padding:1px 5px">$1</code>');
}

Object.assign(window, { HomeScreen });
