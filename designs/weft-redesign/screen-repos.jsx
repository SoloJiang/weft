/* REPOS = the Curator's map. A cross-repo dependency graph (no single repo can
   hold it) + per-repo profiles. This map is the fuel for scope decomposition. */

const NODES = {
  infra:  { x: 95,  y: 80 },
  web:    { x: 440, y: 78 },
  api:    { x: 285, y: 195 },
  mobile: { x: 470, y: 215 },
  tokens: { x: 110, y: 305 },
  docs:   { x: 320, y: 330 },
};
const EDGES = [
  ["web", "api"], ["web", "tokens"], ["mobile", "api"], ["api", "tokens"], ["docs", "api"],
];
const ROLE_GLYPH = { service: IconBox, app: IconLayers, library: IconRepos, infra: IconSettings, docs: IconFile };

function RepoGraph({ sel, onSel }) {
  const curve = (a, b) => {
    const A = NODES[a], B = NODES[b];
    const mx = (A.x + B.x) / 2;
    return `M ${A.x} ${A.y} Q ${mx} ${A.y} ${(A.x+B.x)/2} ${(A.y+B.y)/2} T ${B.x} ${B.y}`;
  };
  return (
    <svg viewBox="0 0 560 400" className="repograph" preserveAspectRatio="xMidYMid meet">
      {EDGES.map(([a, b], i) => {
        const active = sel === a || sel === b;
        return <path key={i} d={curve(a, b)} className="thread-line" style={{ opacity: active ? 0.95 : 0.4, strokeWidth: active ? 1.75 : 1.1 }} />;
      })}
      {window.REPOS.map((r) => {
        const n = NODES[r.id]; const G = ROLE_GLYPH[r.role];
        const on = sel === r.id;
        return (
          <g key={r.id} transform={`translate(${n.x},${n.y})`} className="gnode" onClick={() => onSel(r.id)} style={{ cursor: "pointer" }}>
            <rect x="-46" y="-17" width="92" height="34" rx="9"
              fill={on ? "var(--warp-ghost)" : "var(--surface)"}
              stroke={on ? "var(--warp)" : "var(--border)"}
              strokeWidth={on ? 1.6 : 1} />
            <g transform="translate(-34,-7)" style={{ color: on ? "var(--warp)" : "var(--ink-muted)" }}><G size={14} /></g>
            <text x="-14" y="4" className="gnode-t mono" fill="var(--ink)">{r.name}</text>
          </g>
        );
      })}
    </svg>
  );
}

function ReposScreen({ onDialog }) {
  const [sel, setSel] = React.useState("api");
  const r = window.repo(sel);
  const dependents = window.REPOS.filter((x) => x.deps.includes(sel));
  const IFACE = { api: ["POST /checkout", "GET /cart", "DiscountResult v1"], web: ["—"], tokens: ["tokens.json", "<Button/> <Field/>"], mobile: ["—"], infra: ["deploy(staging|prod)"], docs: ["docs.site"] };

  return (
    <div className="screen">
      <div className="scr-head">
        <div className="col" style={{ gap: 2 }}>
          <div className="row gap2"><IconRepos size={15} className="warp" /><span className="t-eyebrow">CURATOR 维护</span></div>
          <span className="t-h2">6 仓 · 跨仓依赖图</span>
        </div>
        <span className="grow" />
        <span className="chip"><span className="dot" style={{ background: "var(--st-running)" }} /> Curator 已盘点 · 1 天前</span>
        <button className="btn btn-default btn-sm" onClick={() => onDialog && onDialog("add-repo")}><IconPlus size={13} /> 添加仓库</button>
      </div>

      <div className="repos-body">
        <div className="graph-pane">
          <RepoGraph sel={sel} onSel={setSel} />
          <div className="graph-legend">
            <span className="t-meta"><span className="leg-line" /> 依赖关系（来自包清单 · 确定性推导）</span>
            <span className="t-meta">这张图装不进任何单个仓 —— 它是 scope 自动拆解的燃料</span>
          </div>
        </div>

        <aside className="profile-pane scroll-y">
          <div className="row gap2"><span className="mono t-h2">{r.name}</span><span className="chip" style={{ color: "var(--warp)", borderColor: "var(--warp-line)" }}>{r.role}</span></div>
          <div className="prof-oneliner">
            <span className="t-eyebrow">一句话职责 <span className="faint" style={{ textTransform: "none", letterSpacing: 0 }}>· 手填优先于自动推断</span></span>
            <p className="t-body" style={{ marginTop: 4 }}>{r.oneliner}</p>
          </div>

          <div className="prof-grid">
            <div><span className="t-eyebrow">技术栈</span><div className="t-label" style={{ marginTop: 3 }}>{r.stack}</div></div>
            <div><span className="t-eyebrow">Owner</span><div className="t-label" style={{ marginTop: 3 }}>{r.owner}</div></div>
          </div>

          <div className="prof-sec">
            <span className="t-eyebrow">对外接口</span>
            <div className="row gap1" style={{ flexWrap: "wrap", marginTop: 6 }}>
              {IFACE[sel].map((i) => <span key={i} className="mono iface-pill">{i}</span>)}
            </div>
          </div>

          <div className="prof-sec">
            <span className="t-eyebrow">依赖</span>
            <div className="prof-deps">
              {r.deps.length ? r.deps.map((d) => <button key={d} className="dep-chip mono" onClick={() => setSel(d)}><IconArrow size={11} /> {(window.repo(d) || {}).name || d}</button>) : <span className="t-meta faint">无</span>}
            </div>
            <span className="t-eyebrow" style={{ marginTop: 10, display: "block" }}>被依赖</span>
            <div className="prof-deps">
              {dependents.length ? dependents.map((d) => <button key={d.id} className="dep-chip mono" onClick={() => setSel(d.id)}>{d.name} <IconArrow size={11} /></button>) : <span className="t-meta faint">无</span>}
            </div>
          </div>

          <div className="prof-sec">
            <span className="t-eyebrow">约定</span>
            <div className="prof-conv mono">build · test · run 来自 AGENTS.md / 包清单</div>
          </div>

          <div className="prof-foot">
            <span className="t-meta">来源：AGENTS.md › package.json › 目录推断</span>
            <span className="grow" />
            <button className="btn btn-default btn-sm"><IconReplay size={13} /> 重新盘点</button>
          </div>
        </aside>
      </div>
    </div>
  );
}

Object.assign(window, { ReposScreen });
