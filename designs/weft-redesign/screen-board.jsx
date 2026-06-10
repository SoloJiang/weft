/* BOARD = two-level trust dashboard. Workspace (cards = issues) ⇄ Issue (cards = directions
   carrying acceptance signals + expandable provenance). Cards flow themselves;
   the human acts (Approve / Answer / Open / Review / Merge), never drags. */

function ThreadCard({ t, onOpen }) {
  return (
    <button className={"tcard tile" + (t.needs ? " has-need" : "")} onClick={onOpen}>
      <div className="row gap2">
        <span className="grow t-label truncate" style={{ fontWeight: 600 }}>{t.title}</span>
        {t.needs > 0 && <span className="thread-need">{t.needs}</span>}
      </div>
      <div className="row gap2" style={{ marginTop: 5 }}>
        <span className="chip" style={{ height: 18, fontSize: 10 }}>{t.kind}</span>
        <Tool id={t.lead} />
        <span className="grow" />
        <span className="t-meta tnum">{t.progress.done}/{t.progress.total} {L("个子任务", "sub-tasks")}</span>
      </div>
      <div className="tcard-repos">
        {t.writes.map((w) => <span key={w} className="mono repo-pill">{window.repo(w).name}</span>)}
      </div>
      <div className="row gap2 tcard-foot">
        <span className="t-meta"><IconClock size={11} style={{ verticalAlign: "-2px" }} /> {t.age}</span>
        <span className="grow" />
        <span className="auto-tag"><span className="dot" /> {L("自动", "auto")}</span>
      </div>
    </button>
  );
}

function DirectionCard({ d, onSession }) {
  const [open, setOpen] = React.useState(false);
  const act = d.lane === "needs" ? { label: L("处理", "Handle"), cls: "btn-primary", Icon: IconShieldQ }
    : d.lane === "review" ? { label: L("评审 PR", "Review PR"), cls: "btn-weft", Icon: IconMerge }
    : { label: L("打开会话", "Open session"), cls: "btn-default", Icon: IconTerminal };
  return (
    <div className={"dcard" + (d.lane === "needs" ? " alert" : "")}>
      <div className="dcard-head">
        <Tool id={d.tool} />
        <span className="grow t-label truncate" style={{ fontWeight: 600 }}>{d.name}</span>
        <span className="chip" style={{ height: 18, fontSize: 10 }}>{d.role}</span>
      </div>
      <div className="dcard-scope">
        {d.write.map((w) => <span key={w} className="mono repo-pill write"><IconPencil size={10} /> {window.repo(w).name}</span>)}
        {d.read.map((w) => <span key={w} className="mono repo-pill read"><IconEye size={10} /> {window.repo(w).name}</span>)}
      </div>
      <div className="dcard-signals"><Signals s={d.signals} /></div>
      {d.exception === "permission" && (
        <div className="dcard-flag"><IconShieldQ size={13} /> Codex 请求执行命令 — 在「需要你」处理</div>
      )}
      <div className="dcard-foot">
        <button className="prov-toggle" onClick={() => setOpen((o) => !o)}>
          <IconChevR size={13} style={{ transform: open ? "rotate(90deg)" : "none", transition: "transform .15s" }} /> {L("溯源", "provenance")}
        </button>
        <span className="grow" />
        <span className="t-meta mono">+{d.diff.add} −{d.diff.del}</span>
        <button className={"btn btn-sm " + act.cls} onClick={() => onSession(d.id)}><act.Icon size={13} /> {act.label}</button>
      </div>
      {open && (
        <div className="prov fade-in">
          <div className="prov-row"><span className="mono">go test ./...</span><Signal kind="pass" label={`${d.signals.tests[0]}/${d.signals.tests[1]}`} /></div>
          <div className="prov-row"><span className="mono">tsc / buf lint</span><Signal kind={d.signals.type} label="types" /></div>
          <div className="prov-row"><span className="mono">contract: {d.contract.name}</span><Signal kind={d.signals.contract} label={d.contract.state} /></div>
          <div className="prov-row"><span className="mono">review-agent</span><Signal kind={d.signals.review} label={d.signals.review === "pass" ? "approved" : "pending"} /></div>
        </div>
      )}
    </div>
  );
}

function BoardColumn({ lane, children, count }) {
  const l = window.LANES[lane];
  return (
    <div className={"bcol" + (lane === "needs" ? " bcol-needs" : "")}>
      <div className="bcol-head">
        <LaneTag lane={lane} />
        <span className="grow" />
        <span className="t-meta tnum">{count}</span>
      </div>
      <div className="bcol-body">{children}</div>
    </div>
  );
}

function WorkspaceByPhase({ onOpen }) {
  return (
    <div className="board scroll-x">
      {window.LANE_ORDER.map((lane) => {
        const items = window.THREADS.filter((t) => t.lane === lane);
        return (
          <BoardColumn key={lane} lane={lane} count={items.length}>
            {items.map((t) => <ThreadCard key={t.id} t={t} onOpen={() => onOpen(t.id)} />)}
            {items.length === 0 && <div className="bcol-empty t-meta">—</div>}
          </BoardColumn>
        );
      })}
    </div>
  );
}

function ThreadBoardView({ onSession }) {
  return (
    <div className="board scroll-x">
      {window.LANE_ORDER.map((lane) => {
        const items = window.DIRECTIONS.filter((d) => d.lane === lane);
        return (
          <BoardColumn key={lane} lane={lane} count={items.length}>
            {items.map((d) => <DirectionCard key={d.id} d={d} onSession={onSession} />)}
            {items.length === 0 && <div className="bcol-empty t-meta">—</div>}
          </BoardColumn>
        );
      })}
    </div>
  );
}

function BoardScreen({ level, onLevel, onOpenThread, onSession }) {
  return (
    <div className="screen">
      <div className="scr-head">
        <Segmented value={level} onChange={onLevel}
          options={[{ id: "workspace", label: L("Issue · 总览", "Issues · overview"), Icon: IconBoard }, { id: "thread", label: L("子任务 · 结算加优惠码", "Sub-tasks · 结算加优惠码"), Icon: IconLayers }]} />
        <span className="grow" />
        {level === "thread" && <span className="t-meta">{L("卡片自动流转 · 你只需 批准 / 回答 / 打开 / 评审 / 合并", "Cards flow themselves · you Approve / Answer / Open / Review / Merge")}</span>}
      </div>
      <div className="scr-body" style={{ overflow: "hidden" }}>
        {level === "workspace" && <WorkspaceByPhase onOpen={onOpenThread} />}
        {level === "thread" && <ThreadBoardView onSession={onSession} />}
      </div>
    </div>
  );
}

Object.assign(window, { BoardScreen });
