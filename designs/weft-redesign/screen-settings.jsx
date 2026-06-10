/* SETTINGS = general + automation guardrails + the effective-config preview.
   有效配置 = 团队基线 ⊕ 个人覆盖 ⊕ 仓内既有,由工具自带的作用域合并规则本地解析。
   产品价值:让用户看清最终生效了哪些、来自哪层(产品化原则:呈现决策与结果)。 */

const LAYER = {
  team:     { label: "团队基线", color: "var(--warp)",     ghost: "var(--warp-ghost)" },
  personal: { label: "个人",     color: "var(--weft)",     ghost: "var(--weft-ghost)" },
  repo:     { label: "仓内",     color: "var(--ink-muted)", ghost: "transparent" },
};
function LayerChip({ k }) {
  const m = LAYER[k];
  return <span className="chip" style={{ height: 19, fontSize: 10.5, color: m.color, background: m.ghost, borderColor: k === "repo" ? "var(--border)" : "transparent" }}>{m.label}</span>;
}

const EFF_SKILLS = [
  { name: "superpowers:brainstorming", layer: "team",     scope: "项目" },
  { name: "superpowers:writing-plans", layer: "team",     scope: "项目" },
  { name: "security-review",           layer: "team",     scope: "项目" },
  { name: "my-debug-notes",            layer: "personal", scope: "用户" },
  { name: "checkout-lint",             layer: "repo",     scope: "api 仓" },
];
const EFF_RULES = [
  { name: "团队 AGENTS.md(下发)", layer: "team",     scope: "项目", note: "结算域编码规范 + 提交约定" },
  { name: "~/.claude/CLAUDE.md",   layer: "personal", scope: "用户", note: "个人语气 / 快捷偏好" },
  { name: "api/CLAUDE.md",         layer: "repo",     scope: "api 仓", note: "服务自带契约约定" },
];
// an override: personal shadows team for one skill
const OVERRIDE = { name: "code-style", team: "team 版(下发)", personal: "个人版(覆盖生效)" };

function SetRow({ label, hint, children }) {
  return (
    <div className="set-row">
      <div className="col" style={{ gap: 2, minWidth: 0 }}>
        <span className="t-label" style={{ fontWeight: 500 }}>{label}</span>
        {hint && <span className="t-meta">{hint}</span>}
      </div>
      <span className="grow" />
      {children}
    </div>
  );
}
function Toggle({ on, onChange }) {
  return <button className={"toggle" + (on ? " on" : "")} onClick={() => onChange(!on)}><span className="toggle-knob" /></button>;
}

function SettingsScreen({ theme, setTheme }) {
  const [tool, setTool] = React.useState("claude");
  const [lang, setLang] = React.useState("follow");
  const [cons, setCons] = React.useState("mid");
  const [g, setG] = React.useState({ loop: true, merge: true, deploy: true, money: true });
  const tg = (k) => setG((p) => ({ ...p, [k]: !p[k] }));

  return (
    <div className="screen">
      <div className="scr-body">
        <div className="set-wrap">

          {/* A — 通用 */}
          <section className="set-sec">
            <span className="t-eyebrow">通用</span>
            <div className="card set-card">
              <SetRow label="主题" hint="跟随系统,可手动覆盖;chat timeline 与代码区保持高对比">
                <Segmented value={theme} onChange={setTheme}
                  options={[{ id: "dark", label: "深色" }, { id: "light", label: "浅色" }]} />
              </SetRow>
              <SetRow label="默认工具" hint="新 issue 的 lead 默认绑定;每条 issue 可覆盖">
                <Segmented value={tool} onChange={setTool}
                  options={[{ id: "claude", label: "Claude" }, { id: "codex", label: "Codex" }, { id: "opencode", label: "OpenCode" }]} />
              </SetRow>
              <SetRow label="Agent 产出语言" hint="plan / brief / commit / PR 文案的语言;代码与标识符始终英文">
                <Segmented value={lang} onChange={setLang}
                  options={[{ id: "follow", label: "跟随界面" }, { id: "zh", label: "中文" }, { id: "en", label: "English" }]} />
              </SetRow>
            </div>
          </section>

          {/* B — 自动化护栏 */}
          <section className="set-sec">
            <span className="t-eyebrow">自动化护栏 <span className="faint" style={{ textTransform: "none", letterSpacing: 0 }}>· 全自动的前提是跑飞有边界</span></span>
            <div className="card set-card">
              <SetRow label="保守度" hint="越保守越早把事情升级给你;默认中">
                <Segmented value={cons} onChange={setCons}
                  options={[{ id: "low", label: "放手" }, { id: "mid", label: "适中" }, { id: "high", label: "保守" }]} />
              </SetRow>
              <SetRow label="每条 issue 预算上限" hint="token / 时间 / 重试超限即升级,防止烧钱跑飞">
                <span className="set-budget mono">$8.00 · 45min · 3 retries</span>
              </SetRow>
              <SetRow label="相同失败 loop detection" hint="同一报错反复出现即停下并升级">
                <Toggle on={g.loop} onChange={() => tg("loop")} />
              </SetRow>
              <div className="set-irrev">
                <div className="row gap2" style={{ marginBottom: 2 }}><IconShield size={14} className="warp" /><span className="t-label" style={{ fontWeight: 600 }}>不可逆边界 · 需人工确认</span></div>
                <span className="t-meta">这是除工具自身权限外,Weft 唯一的硬关卡;其余全自动流过。</span>
                <div className="set-irrev-rows">
                  <SetRow label="合并受保护分支"><Toggle on={g.merge} onChange={() => tg("merge")} /></SetRow>
                  <SetRow label="生产环境部署"><Toggle on={g.deploy} onChange={() => tg("deploy")} /></SetRow>
                  <SetRow label="删除 / 资金类操作"><Toggle on={g.money} onChange={() => tg("money")} /></SetRow>
                </div>
              </div>
            </div>
          </section>

          {/* C — 有效配置预览 (the star) */}
          <section className="set-sec">
            <span className="t-eyebrow">有效配置预览 <span className="faint" style={{ textTransform: "none", letterSpacing: 0 }}>· 团队基线 ⊕ 个人覆盖 ⊕ 仓内既有</span></span>
            <div className="card set-card">
              <div className="eff-intro t-meta">由工具自带的作用域合并规则在本地解析,无需服务端对账。下面是当前 issue 物化后实际生效的内容,以及各自来自哪一层。</div>

              <div className="eff-group">
                <div className="eff-gh"><IconBolt size={13} className="warp" /> Skills · {EFF_SKILLS.length} 个生效</div>
                {EFF_SKILLS.map((s) => (
                  <div key={s.name} className="eff-row">
                    <span className="mono grow truncate">{s.name}</span>
                    <span className="t-meta nowrap">{s.scope}</span>
                    <LayerChip k={s.layer} />
                  </div>
                ))}
              </div>

              <div className="eff-group">
                <div className="eff-gh"><IconFile size={13} className="warp" /> Rules · AGENTS.md / CLAUDE.md</div>
                {EFF_RULES.map((r) => (
                  <div key={r.name} className="eff-row">
                    <div className="col grow" style={{ gap: 1, minWidth: 0 }}>
                      <span className="t-label truncate" style={{ fontWeight: 500 }}>{r.name}</span>
                      <span className="t-meta truncate">{r.note}</span>
                    </div>
                    <span className="t-meta nowrap">{r.scope}</span>
                    <LayerChip k={r.layer} />
                  </div>
                ))}
              </div>

              <div className="eff-override">
                <IconLayers size={14} style={{ color: "var(--weft)", flex: "0 0 auto" }} />
                <div className="grow">
                  <span className="t-label" style={{ fontWeight: 600 }}>覆盖：</span>
                  <span className="mono">{OVERRIDE.name}</span>
                  <span className="faint"> 同名两层 → </span>
                  <span style={{ textDecoration: "line-through", color: "var(--ink-faint)" }}>{OVERRIDE.team}</span>
                  <span className="faint"> 被 </span>
                  <span className="weft">{OVERRIDE.personal}</span>
                  <span className="faint"> 盖过。高层级优先,与裸 CLI 行为一致。</span>
                </div>
              </div>
            </div>
          </section>

        </div>
      </div>
    </div>
  );
}

Object.assign(window, { SettingsScreen });
