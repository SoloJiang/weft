/* SESSION = Weft-owned chat timeline + the §4.3 interaction layer, in full.
   7-state session machine (body adapts), keyboard-ownership map, interactive
   injection arbitration, approval bar, per-repo diff + protected merge + Inspect. */

const SESS_STATUS = {
  starting:           { cls: "st-inject",  label: "连接中" },
  running:            { cls: "st-running", label: "运行中" },
  "waiting-input":    { cls: "st-waiting", label: "待你输入" },
  "waiting-approval": { cls: "st-waiting", label: "待审批" },
  injecting:          { cls: "st-inject",  label: "注入中" },
  paused:             { cls: "st-idle",    label: "已暂停" },
  exited:             { cls: "st-error",   label: "已退出" },
};
const SESS_ORDER = ["starting", "running", "waiting-input", "waiting-approval", "injecting", "paused", "exited"];

const KEYMAP = [
  { k: "⌘K", who: "产品保留", act: "命令面板" },
  { k: "⌘↵", who: "产品保留", act: "composer 发送整块消息" },
  { k: "⌘[ / ⌘]", who: "产品保留", act: "上 / 下一个面板" },
  { k: "⌘1–9", who: "产品保留", act: "跳到第 N 个会话" },
  { k: "Esc / ⌃C", who: "产品处理", act: "中断 / 停止当前回合" },
  { k: "其余键", who: "composer", act: "/ 斜杠命令 · @ 文件路径 · 普通字符 —— 进入 Weft composer" },
];

function SessionScreen({ onNav, onDialog }) {
  const d = window.DIRECTIONS[0]; // d-api
  const [status, setStatus] = React.useState("running");
  const [statusMenu, setStatusMenu] = React.useState(false);
  const [keys, setKeys] = React.useState(false);
  const [inspect, setInspect] = React.useState(false);
  const [banner, setBanner] = React.useState("queued"); // queued | held | gone
  const [diffW, setDiffW] = React.useState(420);
  const s = SESS_STATUS[status];

  const inject = () => { setStatus("injecting"); setBanner("gone"); };

  return (
    <div className="screen">
      <div className="scr-head sess-head">
        <button className="btn-icon sm" onClick={() => onNav("thread", "t-discount")}><IconChevR size={16} style={{ transform: "rotate(180deg)" }} /></button>
        <Tool id={d.tool} withName />
        <span className="faint">·</span>
        <span className="mono t-label" style={{ color: "var(--ink-muted)" }}>{d.branch}</span>
        <span className={"st " + s.cls}><span className="dot" /> {s.label}</span>
        <span className="grow" />
        <div className="inspect-wrap">
          <button className="btn btn-default btn-sm" onClick={() => { setStatusMenu((v) => !v); setInspect(false); setKeys(false); }}>状态 <IconChevD size={13} /></button>
          {statusMenu && (
            <div className="popover fade-in" style={{ width: 184 }} onMouseLeave={() => setStatusMenu(false)}>
              <div className="pop-label t-eyebrow">模拟会话状态</div>
              {SESS_ORDER.map((k) => (
                <button key={k} className="pop-item" onClick={() => { setStatus(k); setStatusMenu(false); if (k === "running") setBanner("queued"); }}>
                  <span className={"st " + SESS_STATUS[k].cls}><span className="dot" /></span>
                  <span className="grow">{SESS_STATUS[k].label}</span>
                  {k === status && <IconCheck size={13} className="warp" />}
                </button>
              ))}
            </div>
          )}
        </div>
        <button className="btn btn-default btn-sm" onClick={() => { setKeys(true); setStatusMenu(false); setInspect(false); }}>键位</button>
        <div className="inspect-wrap">
          <button className="btn btn-default btn-sm" onClick={() => { setInspect((v) => !v); setStatusMenu(false); }}><IconSettings size={13} /> Inspect</button>
          {inspect && (
            <div className="popover fade-in" onMouseLeave={() => setInspect(false)}>
              <div className="pop-label t-eyebrow">逃生舱（机制在此）</div>
              <button className="pop-item"><IconFolder size={14} /> 在编辑器打开工作副本</button>
              <button className="pop-item"><IconTerminal size={14} /> 在终端打开 cwd</button>
              <button className="pop-item"><IconCopy size={14} /> 复制 resume 命令</button>
              <div className="pop-div" />
              <div className="pop-meta mono">cwd  ~/.weft/wt/checkout-discount-api</div>
              <div className="pop-meta mono">session  a1b2c3d4·claude</div>
            </div>
          )}
        </div>
      </div>

      <div className="sess">
        <section className="sess-main">
          <div className={"chat-frame" + (status === "running" || status === "waiting-input" || status === "injecting" ? " focused" : "")}>
            <div className="chat-top">
              <span className="chat-dot" /><span className="chat-dot" /><span className="chat-dot" />
              <span className="t-meta mono" style={{ marginLeft: 8 }}>~/.weft/wt/checkout-discount-api</span>
              <span className="grow" />
              {(status === "running" || status === "waiting-input") && <span className="focus-tag"><span className="dot" /> typing here</span>}
            </div>

            {/* injection-arbitration banner: queued program message waits for a safe moment */}
            {banner === "queued" && status === "running" && (
              <div className="inject-banner">
                <IconRadio size={13} />
                <span className="grow"><b style={{ fontWeight: 600 }}>web</b> 经协作通道提问已入队 —— 你空闲且非回合中才会注入:<span className="mono">“amountOff 是分还是元？”</span></span>
                <button className="btn btn-sm btn-ghost" onClick={() => setBanner("held")}>暂缓</button>
                <button className="btn btn-sm btn-primary" onClick={inject}><IconSend size={12} /> 注入</button>
              </div>
            )}
            {banner === "held" && status === "running" && (
              <div className="inject-banner" style={{ color: "var(--ink-muted)" }}>
                <IconClock size={13} />
                <span className="grow">已暂缓 1 条注入 —— 不会自动 flush,直到你释放。</span>
                <button className="btn btn-sm btn-default" onClick={() => setBanner("queued")}>释放</button>
              </div>
            )}

            {/* chat body — adapts to status */}
            <div className="chat-body scroll-y">
              {status === "starting" ? (
                <div className="tl tl-sys row gap2"><Spin /> 连接 chat engine · 启动 claude…</div>
              ) : (
                <>
                  {window.TERM.map((l, i) => (
                    <div key={i} className={"tl tl-" + l.t}>
                      {l.t === "user" && <span className="tl-glyph mono">›</span>}
                      {l.t === "tool" && <span className="tl-glyph mono">$</span>}
                      {l.t === "asst" && <span className="tl-ava"><WeaveMark size={13} /></span>}
                      <span className="tl-text">{l.text}</span>
                    </div>
                  ))}
                  {status === "injecting" && (
                    <div className="tl tl-user inject-flash"><span className="tl-glyph mono" style={{ color: "var(--st-inject)" }}>↳</span><span className="tl-text">[coordinator] web 问:amountOff 是分还是元？</span></div>
                  )}
                  {(status === "running" || status === "waiting-input") && <div className="tl tl-cursor"><span className="tl-glyph mono">›</span><span className="caret" /></div>}
                </>
              )}
            </div>

            {/* bottom region — approval bar / overlays / composer per status */}
            {status === "waiting-approval" ? (
              <div className="approval-bar fade-in">
                <IconShieldQ size={15} />
                <div className="grow"><b style={{ fontWeight: 600 }}>claude 请求写入</b> <span className="mono">proto/checkout/v1/discount.proto</span></div>
                <button className="btn btn-sm btn-primary" onClick={() => setStatus("running")}><IconCheck size={13} /> 允许 · y</button>
                <button className="btn btn-sm btn-default">始终</button>
                <button className="btn btn-sm btn-danger" onClick={() => setStatus("running")}><IconX size={13} /> 拒绝 · n</button>
              </div>
            ) : status === "exited" ? (
              <div className="chat-end fade-in">
                <span className="st st-error"><span className="dot" /> 会话已退出(exit 0)</span>
                <span className="grow" />
                <button className="btn btn-sm btn-default"><IconCopy size={13} /> 复制 resume 命令</button>
                <button className="btn btn-sm btn-primary" onClick={() => setStatus("running")}><IconReplay size={13} /> resume 接回</button>
              </div>
            ) : status === "paused" ? (
              <div className="chat-end fade-in">
                <span className="st st-idle"><span className="dot" /> 已暂停 · 外部接管中</span>
                <span className="grow" />
                <button className="btn btn-sm btn-primary" onClick={() => setStatus("running")}><IconReplay size={13} /> re-attach</button>
              </div>
            ) : status === "starting" ? null : (
              <div className="composer chat-composer">
                <input className="composer-input" placeholder={status === "waiting-input" ? "agent 在等你回答 —— 直接输入即可…" : "输入消息;多行内容用 ⌘↵ 整段发送…"} />
                <button className="btn-icon sm" title="@ 插入文件路径"><IconPlus size={15} /></button>
                <button className="btn btn-primary btn-sm"><IconSend size={13} /></button>
              </div>
            )}
          </div>
        </section>

        {/* per-repo diff + gated delivery */}
        <aside className="diffpane" style={{ width: diffW }}>
          <div className="diff-resize" onMouseDown={(e) => {
            const sx = e.clientX, sw = diffW;
            const mv = (ev) => setDiffW(Math.max(320, Math.min(640, sw + (sx - ev.clientX))));
            const up = () => { window.removeEventListener("mousemove", mv); window.removeEventListener("mouseup", up); };
            window.addEventListener("mousemove", mv); window.addEventListener("mouseup", up);
          }} />
          <div className="pr-bar">
            <div className="row gap2">
              <IconBranch size={14} className="mut" />
              <span className="mono t-label" style={{ fontWeight: 600 }}>PR #1284</span>
              <span className="st st-delivered"><span className="dot" /> 已开 PR</span>
              <span className="grow" />
              <button className="btn-icon sm"><IconExternal size={15} /></button>
            </div>
            <div className="row gap1" style={{ marginTop: 8, flexWrap: "wrap" }}>
              <span className="t-meta" style={{ marginRight: 3 }}>仓库 CI / hooks</span>
              <Signal kind="pass" label="build" />
              <Signal kind="pass" label="lint" />
              <Signal kind="pend" label="e2e 运行中" />
              <span className="t-meta faint" style={{ marginLeft: 2 }}>· 权威检查,仓库自带</span>
            </div>
            <div className="row gap2" style={{ marginTop: 8 }}>
              <span className="t-meta mono">2 仓 · 7 文件 · <span className="add">+214</span> <span className="del">−38</span></span>
              <span className="grow" />
              <button className="btn btn-weft btn-sm" title="合并受不可逆边界保护" onClick={() => onDialog && onDialog("merge")}><IconMerge size={13} /> 合并 · 受保护</button>
            </div>
          </div>
          <div className="diff scroll-y">
            {window.DIFF.map((f) => (
              <div key={f.file} className="diff-file">
                <div className="diff-fhead">
                  <IconFile size={13} className="faint" />
                  <span className="mono grow truncate">{f.file}</span>
                  <span className="mono t-meta"><span className="add">+{f.add}</span> <span className="del">−{f.del}</span></span>
                </div>
                {f.hunks.map((h, i) => (
                  <div key={i} className="diff-hunk">
                    <div className="diff-hl mono">{h.h}</div>
                    {h.lines.map((ln, j) => (
                      <div key={j} className={"diff-line mono dl-" + ln[0]}><span className="dl-sign">{ln[0] === "add" ? "+" : ln[0] === "del" ? "−" : ""}</span>{ln[1]}</div>
                    ))}
                  </div>
                ))}
              </div>
            ))}
          </div>
        </aside>
      </div>

      {/* keyboard-ownership map (§4.3) */}
      {keys && (
        <div className="overlay" onClick={() => setKeys(false)}>
          <div className="dialog" style={{ width: 520 }} onClick={(e) => e.stopPropagation()}>
            <div className="dialog-head">
              <span className="dialog-ico"><IconTerminal size={16} /></span>
              <span className="t-h3 grow">键位归属</span>
              <button className="btn-icon sm" onClick={() => setKeys(false)}><IconX size={15} /></button>
            </div>
            <div className="dialog-body">
              <div className="dlg-note" style={{ marginBottom: 2 }}>原则:产品保留导航和发送快捷键;普通输入进入 Weft composer,必要时可在终端接管原生 CLI。</div>
              <div className="keymap">
                {KEYMAP.map((r) => (
                  <div key={r.k} className="keymap-row">
                    <span className="kbd km-key">{r.k}</span>
                    <span className={"km-who " + (r.who === "composer" ? "pass" : "")}>{r.who}</span>
                    <span className="t-meta grow">{r.act}</span>
                  </div>
                ))}
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

Object.assign(window, { SessionScreen });
