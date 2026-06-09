/* ONBOARDING = the ~5-min first-run path to the core wow (ARCHITECTURE 5.3).
   新建 workspace → 加仓(Curator 自动盘点)→ 依赖图自动成形 → 第一个 issue →
   Lead 跨仓拆 scope。Full-screen standalone (pre-workspace), LOOM system. */

const ONB_STEPS = ["欢迎", "工作区", "添加仓库", "依赖图", "第一个 issue", "跨仓拆解"];

const ONB_REPOS = [
  { id: "api",    one: "结算与订单核心服务,对外发布 /cart、/checkout 契约", role: "service" },
  { id: "web",    one: "面向用户的 Web 结算前端,消费 api 的 /checkout",     role: "app" },
  { id: "mobile", one: "iOS / Android 原生结算流程",                          role: "app" },
  { id: "tokens", one: "跨端设计令牌与组件原语",                              role: "library" },
];
const ONB_NODES = { api: [150, 40], web: [40, 120], mobile: [150, 130], tokens: [255, 110] };
const ONB_EDGES = [["web", "api"], ["mobile", "api"], ["api", "tokens"], ["web", "tokens"]];

function OnbGraph() {
  const c = (a, b) => { const A = ONB_NODES[a], B = ONB_NODES[b]; return `M${A[0]} ${A[1]} Q ${(A[0]+B[0])/2} ${A[1]} ${(A[0]+B[0])/2} ${(A[1]+B[1])/2} T ${B[0]} ${B[1]}`; };
  return (
    <svg viewBox="0 0 300 170" className="onb-graph">
      {ONB_EDGES.map(([a, b], i) => <path key={i} d={c(a, b)} className="thread-line" style={{ animation: `draw .5s ${i * 0.12}s var(--ease) both` }} />)}
      {Object.entries(ONB_NODES).map(([id, [x, y]], i) => (
        <g key={id} transform={`translate(${x},${y})`} className="fade-in" style={{ animationDelay: `${0.3 + i * 0.1}s` }}>
          <rect x="-38" y="-14" width="76" height="28" rx="8" fill="var(--surface)" stroke={id === "api" ? "var(--warp)" : "var(--border)"} strokeWidth={id === "api" ? "1.5" : "1"} />
          <text x="0" y="4" textAnchor="middle" className="mono" fill="var(--ink)" style={{ fontSize: 11, fontWeight: 600 }}>{window.repo(id).name}</text>
        </g>
      ))}
    </svg>
  );
}

function OnboardScreen({ onEnter }) {
  const [step, setStep] = React.useState(0);
  const [task, setTask] = React.useState("给结算流程加优惠码:用户在结算页输入优惠码,实时校验并折扣");
  const next = () => setStep((s) => Math.min(s + 1, ONB_STEPS.length - 1));
  const back = () => setStep((s) => Math.max(s - 1, 0));

  const Body = () => {
    if (step === 0) return (
      <div className="onb-hero fade-in">
        <WeaveMark size={56} />
        <h1 className="onb-wordmark">Weft</h1>
        <p className="t-h3 mut" style={{ fontWeight: 400 }}>本地优先 · agent 驱动的多仓交付中心</p>
        <p className="t-body faint" style={{ maxWidth: "42ch", marginTop: 6 }}>给一个 Task,Weft 自动决定改哪些仓、派 agent、验证、出 PR。下面 5 步带你从零到第一次跨仓拆解。</p>
      </div>
    );
    if (step === 1) return (
      <div className="onb-pane fade-in">
        <h2 className="t-h2">新建工作区</h2>
        <p className="t-meta">工作区是一份逻辑仓库清单,不是父仓,也不会拷贝你的代码。</p>
        <label className="onb-field">
          <span className="t-eyebrow">工作区名称</span>
          <input className="onb-input" defaultValue="结算改版" />
        </label>
      </div>
    );
    if (step === 2) return (
      <div className="onb-pane fade-in">
        <h2 className="t-h2">添加仓库 · Curator 自动盘点</h2>
        <p className="t-meta">按 .git 引用,不拷贝。加入后 Curator 只读盘点一句话职责,你可改——手填优先于自动推断。</p>
        <div className="onb-repos">
          {ONB_REPOS.map((r, i) => (
            <div key={r.id} className="onb-repo fade-in" style={{ animationDelay: `${i * 0.08}s` }}>
              <span className="onb-repo-check"><IconCheck size={13} /></span>
              <div className="grow" style={{ minWidth: 0 }}>
                <div className="row gap2"><span className="mono t-label" style={{ fontWeight: 600 }}>{window.repo(r.id).name}</span><span className="chip" style={{ height: 17, fontSize: 10 }}>{r.role}</span></div>
                <div className="t-meta truncate">{r.one}</div>
              </div>
            </div>
          ))}
          <div className="onb-repo add"><IconPlus size={14} className="faint" /><span className="t-meta">~/code/ 选更多本地仓…</span></div>
        </div>
      </div>
    );
    if (step === 3) return (
      <div className="onb-pane fade-in">
        <h2 className="t-h2">依赖图自动成形</h2>
        <p className="t-meta">从包清单连边(确定性)。这张图装不进任何单个仓 —— 它是后面 scope 拆解的燃料。</p>
        <div className="onb-graph-wrap"><OnbGraph /></div>
      </div>
    );
    if (step === 4) return (
      <div className="onb-pane fade-in">
        <h2 className="t-h2">新建第一个 issue</h2>
        <p className="t-meta">给一个 Task(需求 / bug / 重构都行)。Lead 会自动归类,再读地图划定 scope。</p>
        <div className="onb-task">
          <textarea className="composer-input" rows={3} value={task} onChange={(e) => setTask(e.target.value)} />
        </div>
      </div>
    );
    return (
      <div className="onb-pane fade-in">
        <div className="row gap2"><IconSpark size={16} className="warp" /><h2 className="t-h2">Lead 跨仓拆解 scope</h2></div>
        <p className="t-meta">一个 Task → 改哪些仓、谁干、什么顺序 —— 全自动推断,你只需纠正写集合。这就是 Weft 的核心。</p>
        <div className="onb-scope">
          {window.SCOPE.inferred.map((s, i) => (
            <div key={s.repo} className={"onb-lane r-" + s.role + " fade-in"} style={{ animationDelay: `${i * 0.1}s` }}>
              <span className="onb-thread" />
              <span className="mono onb-lane-repo">{window.repo(s.repo).name}</span>
              <ScopeRole role={s.role} small />
              <span className="grow t-meta truncate">{s.dir || s.reason}</span>
              {s.tool && <Tool id={s.tool} />}
            </div>
          ))}
        </div>
        <div className="row gap2 onb-scope-sum">
          <span className="foot-sum"><span className="weft-dot" /> 3 写</span>
          <span className="foot-sum"><span className="warp-dot" /> 2 只读</span>
          <span className="foot-sum faint"><span className="none-dot" /> 1 不涉及</span>
          <span className="grow" />
          <span className="t-meta">api 先发契约 → web / mobile 并行接入</span>
        </div>
      </div>
    );
  };

  const last = step === ONB_STEPS.length - 1;
  return (
    <div className="onb">
      <div className="onb-top">
        <div className="row gap2"><WeaveMark size={18} /><span className="t-label" style={{ fontWeight: 600 }}>Weft</span><span className="t-meta">首用引导</span></div>
        <div className="onb-steps">
          {ONB_STEPS.map((s, i) => (
            <span key={i} className={"onb-step" + (i === step ? " on" : "") + (i < step ? " done" : "")}>
              <span className="onb-dot">{i < step ? <IconCheck size={10} /> : i + 1}</span>
              <span className="onb-step-label">{s}</span>
            </span>
          ))}
        </div>
        <button className="btn btn-ghost btn-sm" onClick={onEnter}>跳过</button>
      </div>

      <div className="onb-stage"><Body /></div>

      <div className="onb-foot">
        <button className="btn btn-default" onClick={back} disabled={step === 0}>上一步</button>
        <span className="grow" />
        <span className="t-meta">{step + 1} / {ONB_STEPS.length}</span>
        {last
          ? <button className="btn btn-weft" onClick={onEnter}><IconArrow size={14} /> 进入工作区</button>
          : <button className="btn btn-primary" onClick={next}>{step === 0 ? "开始" : "下一步"} <IconArrow size={14} /></button>}
      </div>
    </div>
  );
}

Object.assign(window, { OnboardScreen });
