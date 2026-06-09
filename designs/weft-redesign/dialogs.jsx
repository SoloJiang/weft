/* Dialog system — the modal layer the prototype was missing.
   Generic shell + 4 real product dialogs incl. the irreversible-boundary
   merge gate (PRODUCT.md: the one human gate besides tools' own prompts). */

function Dialog({ title, Icon, tone, onClose, children, footer, width = 460 }) {
  return (
    <div className="overlay" onClick={onClose}>
      <div className="dialog" style={{ width }} onClick={(e) => e.stopPropagation()} role="dialog" aria-modal="true">
        <div className="dialog-head">
          {Icon && <span className={"dialog-ico" + (tone ? " t-" + tone : "")}><Icon size={16} /></span>}
          <span className="t-h3 grow">{title}</span>
          <button className="btn-icon sm" onClick={onClose}><IconX size={15} /></button>
        </div>
        <div className="dialog-body">{children}</div>
        <div className="dialog-foot">{footer}</div>
      </div>
    </div>
  );
}

function Field({ label, hint, children }) {
  return (
    <label className="dlg-field">
      <span className="t-eyebrow" style={{ textTransform: "none", letterSpacing: 0, fontWeight: 600, color: "var(--ink-muted)" }}>{label}</span>
      {children}
      {hint && <span className="t-meta">{hint}</span>}
    </label>
  );
}

function NewIssueDialog({ onClose, onNav, onToast }) {
  return (
    <Dialog title="新建 issue" Icon={IconThread} onClose={onClose} width={520}
      footer={<>
        <span className="t-meta grow">创建后 Lead 自动归类 → 读地图 → 划定 scope → 拆子任务派发</span>
        <button className="btn btn-ghost" onClick={onClose}>取消</button>
        <button className="btn btn-primary" onClick={() => { onClose(); onToast && onToast("success", "已创建 issue · Lead 正在拆 scope"); onNav("scope"); }}>创建 issue</button>
      </>}>
      <Field label="标题"><input className="dlg-input" defaultValue="给结算流程加优惠码" /></Field>
      <Field label="Task" hint="需求 / bug / 重构 / spike / 链接都行 —— Lead 会自动归类(PRD 只是一种)">
        <textarea className="dlg-input dlg-area" rows={3} defaultValue="用户在结算页输入优惠码,实时校验并按规则折扣。" />
      </Field>
      <div className="row gap3">
        <Field label="Lead 工具">
          <select className="dlg-input dlg-select"><option>Claude Code</option><option>Codex</option><option>OpenCode</option></select>
        </Field>
        <Field label="产出语言">
          <select className="dlg-input dlg-select"><option>跟随界面</option><option>中文</option><option>English</option></select>
        </Field>
      </div>
    </Dialog>
  );
}

function AddRepoDialog({ onClose, onToast }) {
  return (
    <Dialog title="添加仓库" Icon={IconRepos} onClose={onClose} width={500}
      footer={<>
        <button className="btn btn-ghost" onClick={onClose}>取消</button>
        <button className="btn btn-primary" onClick={() => { onClose(); onToast && onToast("success", "已添加 payments · Curator 盘点中…"); }}><IconCheck2 size={14} /> 添加并盘点</button>
      </>}>
      <Field label="本地 .git 路径" hint="按引用,不拷贝代码;支持 monorepo 子目录">
        <div className="dlg-path">
          <input className="dlg-input grow" defaultValue="~/code/payments" />
          <button className="btn btn-default"><IconFolder size={14} /> 选择…</button>
        </div>
      </Field>
      <div className="dlg-detect">
        <IconBox size={15} className="warp" />
        <div className="grow">
          <div className="row gap2"><span className="mono t-label" style={{ fontWeight: 600 }}>payments</span><span className="chip" style={{ height: 18, fontSize: 10, color: "var(--warp)", borderColor: "var(--warp-line)" }}>service</span></div>
          <div className="t-meta">已识别为 Go 服务 · Curator 将只读盘点一句话职责(你可改)</div>
        </div>
      </div>
      <div className="dlg-note"><IconRepos size={13} /> 加入后跨仓依赖图自动 reconcile,在飞 issue 检测到依赖会提示关联。</div>
    </Dialog>
  );
}

function MergeDialog({ onClose, onToast }) {
  const checks = [["42/42 测试", "pass"], ["类型", "pass"], ["契约一致", "pass"], ["review-agent", "pass"], ["仓库 CI / hooks", "pass"]];
  return (
    <Dialog title="确认合并 · 不可逆边界" Icon={IconMerge} tone="weft" onClose={onClose} width={520}
      footer={<>
        <button className="btn btn-ghost" onClick={onClose}>取消</button>
        <button className="btn btn-weft" onClick={() => { onClose(); onToast && onToast("success", "已合并 PR #1284 → main · 清理 3 个工作副本"); }}><IconMerge size={14} /> 确认合并到 main</button>
      </>}>
      <div className="dlg-merge-head">
        <div className="row gap2"><IconBranch size={14} className="mut" /><span className="mono t-label" style={{ fontWeight: 600 }}>PR #1284</span><IconArrow size={13} className="faint" /><span className="mono t-label">main</span><span className="chip" style={{ height: 18, fontSize: 10 }}>受保护</span></div>
        <span className="t-meta">api · 7 文件 · <span className="add">+214</span> <span className="del">−38</span></span>
      </div>
      <div className="dlg-checks">
        {checks.map((c) => <span key={c[0]} className="signal pass"><IconCheck /> {c[0]}</span>)}
      </div>
      <div className="dlg-warn">
        <IconShield size={15} />
        <div>合并到受保护分支是<b style={{ fontWeight: 600 }}>不可逆操作</b>,这是 Weft 唯一会拦你的边界(其余全自动流过)。合并后将自动关闭 3 个子任务、清理对应工作副本。</div>
      </div>
    </Dialog>
  );
}

function DeleteIssueDialog({ onClose, onToast }) {
  const branches = ["ws/checkout/discount/api", "ws/checkout/discount/web", "ws/checkout/discount/mobile"];
  return (
    <Dialog title="删除该 issue?" Icon={IconWarn} tone="danger" onClose={onClose} width={500}
      footer={<>
        <button className="btn btn-ghost" onClick={onClose}>取消</button>
        <button className="btn btn-danger" style={{ border: "1px solid color-mix(in oklch, var(--st-error) 40%, transparent)" }} onClick={() => { onClose(); onToast && onToast("info", "已删除 issue 及 3 个工作副本", { label: "撤销" }); }}><IconX size={14} /> 删除 issue 及工作副本</button>
      </>}>
      <p className="t-body">将一并删除 <b>3 个工作副本</b>及其分支,<b style={{ color: "var(--st-error)" }}>未合并的改动会丢失</b>:</p>
      <div className="dlg-list">
        {branches.map((b) => <div key={b} className="dlg-list-row"><IconBranch size={13} className="faint" /><span className="mono">{b}</span></div>)}
      </div>
      <div className="dlg-note"><IconCheck size={13} /> 已开的 PR (#1284) 与已合并的提交不受影响。</div>
    </Dialog>
  );
}

function Dialogs({ open, onClose, onNav, onToast }) {
  if (!open) return null;
  if (open === "new-issue") return <NewIssueDialog onClose={onClose} onNav={onNav} onToast={onToast} />;
  if (open === "add-repo") return <AddRepoDialog onClose={onClose} onToast={onToast} />;
  if (open === "merge") return <MergeDialog onClose={onClose} onToast={onToast} />;
  if (open === "delete-issue") return <DeleteIssueDialog onClose={onClose} onToast={onToast} />;
  return null;
}

Object.assign(window, { Dialog, Dialogs });
