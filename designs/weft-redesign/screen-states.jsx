/* STATES = 状态与边界目录。把核心界面会遇到的 空 / 加载 / 错误 / 边界 状态用真实
   组件 + token 集中呈现,作为实现用的状态规格。体现 DESIGN 的状态语义 +
   PRODUCT 的「失败可读 + 就地逃生舱」原则。 */

function Tile({ label, span, children }) {
  return (
    <div className="st-tile" style={span ? { gridColumn: "1 / -1" } : null}>
      <div className="st-tile-label t-eyebrow">{label}</div>
      <div className="st-tile-body">{children}</div>
    </div>
  );
}
function Spin() { return <span className="spin" />; }

function StatesScreen() {
  const SESS = [
    { cls: "st-inject", label: "启动中", glyph: "↳" },
    { cls: "st-running", label: "运行中", glyph: "●", pulse: true },
    { cls: "st-waiting", label: "待输入", glyph: "◐" },
    { cls: "st-waiting", label: "待审批", glyph: "⚠" },
    { cls: "st-inject", label: "注入中", glyph: "↳" },
    { cls: "st-idle", label: "空闲", glyph: "○" },
    { cls: "st-delivered", label: "已交付", glyph: "◆" },
    { cls: "st-error", label: "已退出 / 异常", glyph: "✕" },
  ];
  return (
    <div className="screen">
      <div className="scr-body">
        <div className="states-wrap">

          <Tile label="会话状态链 · 色彩永不单独表意(配字形 + 标签)">
            <div className="state-chips">
              {SESS.map((s, i) => (
                <span key={i} className={"st " + s.cls}>
                  <span className="dot" /> {s.label}
                </span>
              ))}
            </div>
          </Tile>

          <Tile label="信任信号 · 四态(看板卡 / 控制台)">
            <div className="col" style={{ gap: 10 }}>
              <div className="row gap2"><span className="state-cap">全绿</span><Signals s={{ tests: [42, 42], type: "pass", contract: "pass", review: "pass" }} /></div>
              <div className="row gap2"><span className="state-cap">部分</span><Signals s={{ tests: [27, 31], type: "pass", contract: "pass", review: "pend" }} /></div>
              <div className="row gap2"><span className="state-cap">失败</span><Signals s={{ tests: [12, 31], type: "fail", contract: "pass", review: "pend" }} /></div>
              <div className="row gap2"><span className="state-cap">待跑</span><Signals s={{ tests: [0, 22], type: "pend", contract: "pend", review: "pend" }} /></div>
            </div>
          </Tile>

          <Tile label="加载态 · 进行中的活计有具体说明,不是空转 spinner">
            <div className="col" style={{ gap: 11 }}>
              <div className="row gap2"><Spin /> <span className="t-label">Curator 正在盘点 <span className="mono">api</span> …</span></div>
              <div className="row gap2"><Spin /> <span className="t-label">Lead 正在跨仓拆解 scope …</span></div>
              <div className="row gap2"><Spin /> <span className="t-label">运行验收检查 <span className="mono faint">go test ./… · 18/42</span></span></div>
            </div>
          </Tile>

          <Tile label="空态 · 新工作区还没有 issue">
            <div className="state-empty">
              <span className="state-empty-ico"><IconThread size={20} /></span>
              <span className="t-label" style={{ fontWeight: 600 }}>还没有 issue</span>
              <span className="t-meta">给一个 Task,Lead 会自动拆 scope 并派发。</span>
              <button className="btn btn-primary btn-sm" style={{ marginTop: 4 }}><IconPlus size={13} /> 新建 issue</button>
            </div>
          </Tile>

          <Tile label="空态 · 看板列(没有卡)">
            <div className="bcol" style={{ width: "100%", maxWidth: 260 }}>
              <div className="bcol-head"><LaneTag lane="review" /><span className="grow" /><span className="t-meta tnum">0</span></div>
              <div className="bcol-body"><div className="bcol-empty t-meta">暂无 · 卡片会自动流入</div></div>
            </div>
          </Tile>

          <Tile label="错误 + 逃生舱 · 失败用产品语言说清,就地递上真路径 / 开终端" span>
            <div className="state-err">
              <span className="state-err-ico"><IconWarn size={16} /></span>
              <div className="grow">
                <div className="t-label" style={{ fontWeight: 600 }}>建立工作副本失败 · <span className="mono">web-app</span></div>
                <div className="t-meta" style={{ marginTop: 2 }}>同名分支 <span className="mono">ws/checkout/discount/web</span> 已在另一个工作副本检出 —— 同一分支不能同时检出两次。</div>
                <div className="state-err-acts">
                  <button className="btn btn-default btn-sm"><IconTerminal size={13} /> 在终端打开</button>
                  <button className="btn btn-default btn-sm"><IconFile size={13} /> 查看日志</button>
                  <button className="btn btn-default btn-sm"><IconCopy size={13} /> 复制路径</button>
                  <button className="btn btn-primary btn-sm"><IconReplay size={13} /> 换分支名重试</button>
                </div>
              </div>
            </div>
          </Tile>

          <Tile label="待你处理 dock · 有异常 / 全清 两态" span>
            <div className="col" style={{ gap: 10 }}>
              <div className="state-dock active">
                <span className="needs-pip" style={{ background: "var(--st-waiting)" }}>3</span>
                <span className="t-h3" style={{ color: "var(--st-waiting)" }}>待你处理</span>
                <span className="mut t-label" style={{ marginLeft: 4 }}><IconShieldQ size={13} style={{ verticalAlign: "-2px", color: "var(--st-waiting)" }} /> Codex 请求执行命令 <span className="faint">· 结算加优惠码</span></span>
              </div>
              <div className="state-dock clear">
                <span className="st st-running"><span className="dot" /></span>
                <span className="t-label mut">自动流转中 · 暂无待你处理的事项</span>
                <span className="grow" />
                <span className="t-meta">5 个 issue 自动推进中</span>
              </div>
            </div>
          </Tile>

          <Tile label="低可信度边界 · 仓库无可执行检查时,诚实标注" span>
            <div className="state-lowconf">
              <IconWarn size={14} style={{ color: "var(--st-waiting)", flex: "0 0 auto" }} />
              <div className="grow">
                <span className="t-label" style={{ fontWeight: 600 }}><span className="mono">docs</span> 无测试 / 类型检查</span>
                <span className="t-meta"> —— 验收退到 review-agent,卡上标「低可信度」,更易升级,可让 lead 指示补测试。「绿」在这里不代表已验证。</span>
              </div>
              <span className="chip" style={{ color: "var(--st-waiting)", borderColor: "var(--border)" }}>低可信度</span>
            </div>
          </Tile>

        </div>
      </div>
    </div>
  );
}

Object.assign(window, { StatesScreen });
