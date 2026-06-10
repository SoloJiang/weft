/* NOTES = the strategy, made self-contained inside the artifact: the new
   "LOOM" identity + the six structural moves (current → recommended). */

function Swatch({ v, name, note }) {
  return (
    <div className="swatch">
      <span className="swatch-chip" style={{ background: v }} />
      <div className="col" style={{ gap: 1 }}><span className="t-label mono" style={{ fontWeight: 600 }}>{name}</span><span className="t-meta">{note}</span></div>
    </div>
  );
}

function Move({ n, title, from, to }) {
  return (
    <div className="move card">
      <span className="move-n">{n}</span>
      <div className="grow">
        <div className="t-h3" style={{ marginBottom: 8 }}>{title}</div>
        <div className="move-ba">
          <div className="ba from"><span className="t-eyebrow">现状</span><p className="t-body">{from}</p></div>
          <IconArrow size={16} className="warp move-arrow" />
          <div className="ba to"><span className="t-eyebrow" style={{ color: "var(--weft)" }}>建议</span><p className="t-body">{to}</p></div>
        </div>
      </div>
    </div>
  );
}

function NotesScreen() {
  return (
    <div className="screen">
      <div className="scr-body">
        <div className="notes-wrap">

          <section className="notes-hero">
            <span className="t-eyebrow">视觉身份</span>
            <h1 className="t-display" style={{ marginTop: 6 }}>LOOM — 一间会编织的控制室</h1>
            <p className="t-body mut" style={{ maxWidth: "64ch", marginTop: 8 }}>
              产品名 <b style={{ color: "var(--ink)" }}>Weft（纬线）</b>本身就是最强、且竞品无法占用的隐喻：多个并行 issue 如经纬交织，最终收束为一个交付结果。
              新视觉把它落到底层——暖石墨的夜间控制室基底，冷<b className="warp">青色「经线」</b>承载结构（issue / 子任务 / 链接），
              暖<b className="weft">珊瑚「纬线」</b>标记收束与交付。摆脱当前略显通用的「AI 仪表盘」紫黑感，换成一套有观点、可拥有的工程美学。
            </p>
            <div className="palette-colors">
              <Swatch v="var(--bg)" name="surface" note="暖石墨基底" />
              <Swatch v="var(--warp)" name="warp" note="青·结构/经线" />
              <Swatch v="var(--weft)" name="weft" note="珊瑚·收束/纬线" />
              <Swatch v="var(--st-running)" name="running" note="绿·运行" />
              <Swatch v="var(--st-waiting)" name="needs" note="琥珀·需要你" />
              <Swatch v="var(--st-error)" name="error" note="红·异常" />
            </div>
            <div className="type-spec">
              <div><span className="t-display">Geist</span><span className="t-meta"> 显示 / 标题 / 正文</span></div>
              <div><span className="t-h2 mono">Geist Mono</span><span className="t-meta"> 路径 · 分支 · diff · 契约</span></div>
              <div className="row gap2"><span className="st st-running"><span className="dot" />running</span><span className="st st-waiting"><span className="dot" />needs</span><span className="st st-delivered"><span className="dot" />delivered</span><span className="t-meta">色彩永不单独表意 — 永远配字形+标签</span></div>
            </div>
          </section>

          <section>
            <span className="t-eyebrow">六个结构性动作 — 把「看 agent 干活的仪表盘」改造成「监督自动交付线的控制室」</span>
            <div className="moves">
              <Move n="1" title="重心转移：以对话为家"
                from="默认入口是看板（一块要去『看』的板）；Lead 对话被埋在 issue 里的一个 tab。"
                to="默认入口 = Lead 控制台。任务进，scope / brief / 升级以结构化卡片在对话流里出。看板退为同伴视图。" />
              <Move n="2" title="看板 → 信任仪表盘"
                from="卡片是任务条目，靠人拖动在列间流转，验证信号几乎不可见。"
                to="卡片是信任凭证：acceptance（tests x/y · 契约 · review）置于显眼处 + 可展开 provenance；卡自动流转，人只 Approve/Answer/Open/Review/Merge。" />
              <Move n="3" title="「需要你」成为引力中心"
                from="Needs-you 是众多界面之一，并非永远最显眼。"
                to="常驻 dock，置顶于每个界面，跨全部 issue 聚合异常；空态读作『自动流转中』，而非空白。" />
              <Move n="4" title="把核心 wow 拍成电影"
                from="scope 确认是一个朴素步骤；依赖顺序与契约握手不可见。"
                to="Task → 编织式 scope 地图（写/只读/不涉及随仓亮灭）→ 依赖顺序与契约先行 → 唯一人工 gate，可纠正、会学习。" />
              <Move n="5" title="跨仓地图前置"
                from="scope 分解依赖临时读仓，仓库职责和依赖关系容易被埋进对话上下文。"
                to="Repo Profile + 依赖图成为 Workspace 的一等资源；Lead 用紧凑地图拆 scope，用户可在仓库地图里检查和修正。" />
              <Move n="6" title="机制隐入，决策在前"
                from="worktree / headless 进程 / diff 面板等机制与产品决策混在同一层级。"
                to="机制（worktree/headless engine/MCP/add-dir）退进 Inspect 逃生舱；任务、scope、分支/PR/diff、工具选择留在台前；失败可读、随手可逃。" />
            </div>
          </section>

        </div>
      </div>
    </div>
  );
}

Object.assign(window, { NotesScreen });
