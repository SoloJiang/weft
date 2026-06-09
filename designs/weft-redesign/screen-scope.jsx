/* SCOPE = the wow. One Task → "these repos, this split, in this order".
   The weave made literal: write repos get a coral weft line, read repos a teal
   warp line, untouched repos a dim dashed line. The one human gate: correct the
   write set, then a single Create materializes worktrees. Reads are unmanaged. */

function ScopeScreen({ onNav }) {
  const [roles, setRoles] = React.useState(() => {
    const m = {}; window.SCOPE.inferred.forEach((s) => (m[s.repo] = s.role)); return m;
  });
  const cycle = (id) => setRoles((r) => ({ ...r, [id]: r[id] === "write" ? "read" : r[id] === "read" ? "none" : "write" }));
  const writes = window.SCOPE.inferred.filter((s) => roles[s.repo] === "write");
  const reads = window.SCOPE.inferred.filter((s) => roles[s.repo] === "read");
  const nones = window.SCOPE.inferred.filter((s) => roles[s.repo] === "none");

  return (
    <div className="screen">
      <div className="scr-head">
        <IconSpark size={15} className="warp" />
        <span className="t-eyebrow">跨仓 SCOPE 拆解 · 核心能力</span>
        <span className="grow" />
        <button className="btn btn-default btn-sm"><IconReplay size={13} /> 重新拆解</button>
      </div>

      <div className="scr-body">
        <div className="scope-wrap">
          {/* task node */}
          <div className="scope-task fade-in">
            <span className="scope-task-ico"><IconBolt size={16} /></span>
            <div className="grow col" style={{ gap: 2 }}>
              <span className="t-meta">输入的 Task</span>
              <span className="t-h3">给结算流程加优惠码</span>
            </div>
            <span className="chip" style={{ color: "var(--warp)", borderColor: "var(--warp-line)" }}>feature</span>
            <span className="chip">已读地图 · 尚未建副本</span>
          </div>

          <div className="scope-axis"><span className="scope-axis-label t-eyebrow">Lead 推断 — 你可纠正写集合</span></div>

          {/* woven lanes */}
          <div className="scope-lanes">
            {window.SCOPE.inferred.map((s, i) => {
              const role = roles[s.repo];
              const r = window.repo(s.repo);
              const blocked = s.order === 2;
              return (
                <div key={s.repo} className={"lane r-" + role + " fade-in"} style={{ animationDelay: (i * 35) + "ms" }}>
                  <span className="lane-order">{role === "write" ? <span className="ord">{s.order}</span> : <span className="ord-none">·</span>}</span>
                  <span className="lane-weave"><span className="lane-thread" /></span>
                  <div className="lane-id">
                    <span className="mono lane-repo">{r.name}</span>
                    <span className="t-meta">{r.role} · {r.stack}</span>
                  </div>
                  <button className="lane-role" onClick={() => cycle(s.repo)} title="点击切换 写 / 只读 / 不涉及">
                    <ScopeRole role={role} />
                  </button>
                  <div className="lane-mid">
                    {role === "write" ? (
                      <div className="row gap2">
                        <span className="t-label" style={{ fontWeight: 500 }}>{s.dir || "新子任务"}</span>
                        {s.tool && <Tool id={s.tool} withName />}
                        {blocked && <span className="chip" style={{ height: 19, fontSize: 10, color: "var(--warp)", borderColor: "var(--warp-line)" }}><IconClock size={11} /> 等 api 契约</span>}
                      </div>
                    ) : (
                      <span className="t-meta">{s.reason}</span>
                    )}
                  </div>
                  {role === "write" && <span className="lane-reason t-meta truncate">{s.reason}</span>}
                </div>
              );
            })}
          </div>

          {/* dependency note */}
          <div className="scope-dep card">
            <IconFlow size={15} className="warp" />
            <div className="grow">
              <span className="t-label" style={{ fontWeight: 600 }}>执行顺序：</span>
              <span className="mono"> api</span> <span className="faint">发布</span> <span className="weft">DiscountResult</span> <span className="faint">契约 →</span> <span className="mono">web</span> <span className="faint">/</span> <span className="mono">mobile</span> <span className="faint">并行接入。依赖来自跨仓依赖图，非人工标注。</span>
            </div>
          </div>
        </div>
      </div>

      {/* footer: the single human gate */}
      <div className="scope-foot">
        <div className="row gap3 grow" style={{ flexWrap: "wrap" }}>
          <span className="foot-sum"><span className="weft-dot" /> <b>{writes.length}</b> 个写 → 建工作副本</span>
          <span className="foot-sum"><span className="warp-dot" /> <b>{reads.length}</b> 个只读 → 不建副本（agent 可自由读）</span>
          <span className="foot-sum faint"><span className="none-dot" /> <b>{nones.length}</b> 个不涉及</span>
        </div>
        <span className="t-meta nowrap" style={{ marginRight: 8 }}>这是唯一的人工确认；其余全自动</span>
        <button className="btn btn-weft" onClick={() => onNav("thread", "t-discount")}>
          <IconBranch size={14} /> 建立 {writes.length} 个工作副本并派发
        </button>
      </div>
    </div>
  );
}

Object.assign(window, { ScopeScreen });
