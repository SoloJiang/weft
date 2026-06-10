/* Mock scenario: workspace "结算改版 (Checkout revamp)".
   Coherent story across surfaces: a Task → cross-repo scope → heterogeneous
   workers → trust signals → live exceptions. */

const TOOLS = {
  claude:   { name: "Claude Code", glyph: "C", cls: "glyph-claude" },
  codex:    { name: "Codex",       glyph: "Cx", cls: "glyph-codex" },
  opencode: { name: "OpenCode",    glyph: "Oc", cls: "glyph-opencode" },
};

const REPOS = [
  { id: "api",    name: "api",           role: "service", stack: "Go · gRPC", oneliner: "结算与订单核心服务，对外发布 /cart、/checkout 契约", deps: ["tokens"], owner: "platform" },
  { id: "web",    name: "web-app",       role: "app",     stack: "React · TS", oneliner: "面向用户的 Web 结算前端，消费 api 的 /checkout", deps: ["api", "tokens"], owner: "growth" },
  { id: "mobile", name: "mobile",        role: "app",     stack: "Swift · Kotlin", oneliner: "iOS / Android 原生结算流程", deps: ["api"], owner: "mobile" },
  { id: "tokens", name: "design-tokens", role: "library", stack: "TS", oneliner: "跨端设计令牌与组件原语", deps: [], owner: "design" },
  { id: "infra",  name: "infra",         role: "infra",   stack: "Terraform", oneliner: "部署流水线与环境编排", deps: [], owner: "platform" },
  { id: "docs",   name: "docs",          role: "docs",    stack: "Markdown", oneliner: "产品与 API 文档站", deps: ["api"], owner: "docs" },
];
const repo = (id) => REPOS.find((r) => r.id === id);

/* directions of the active thread "结算加优惠码" */
const DIRECTIONS = [
  {
    id: "d-api", name: "优惠码契约 + 校验端点", tool: "claude", branch: "ws/checkout/discount/api",
    write: ["api"], read: ["tokens"], mandate: "plan+impl", lane: "review", role: "契约生产者",
    objective: "新增 POST /checkout/discount 校验端点，发布 DiscountResult 契约供前端消费。",
    contract: { name: "DiscountResult v1", state: "published" },
    deps: [], blocks: ["d-web", "d-mobile"],
    signals: { tests: [42, 42], type: "pass", contract: "pass", review: "pass" },
    diff: { files: 7, add: 214, del: 38 },
    pr: { num: 1284, state: "open" },
  },
  {
    id: "d-web", name: "Web 结算接入优惠码", tool: "codex", branch: "ws/checkout/discount/web",
    write: ["web"], read: ["api", "tokens"], mandate: "impl-only", lane: "needs", role: "消费者",
    objective: "结算页加入优惠码输入与即时校验，接 DiscountResult 契约。",
    contract: { name: "DiscountResult v1", state: "consuming" },
    deps: ["d-api"], blocks: [],
    signals: { tests: [27, 31], type: "pass", contract: "pass", review: "pend" },
    diff: { files: 11, add: 318, del: 64 },
    exception: "permission",
  },
  {
    id: "d-mobile", name: "移动端结算接入优惠码", tool: "opencode", branch: "ws/checkout/discount/mobile",
    write: ["mobile"], read: ["api"], mandate: "plan+impl", lane: "working", role: "消费者",
    objective: "iOS / Android 结算流程接入优惠码校验。",
    contract: { name: "DiscountResult v1", state: "consuming" },
    deps: ["d-api"], blocks: [],
    signals: { tests: [9, 22], type: "pend", contract: "pend", review: "pend" },
    diff: { files: 5, add: 96, del: 12 },
  },
];

/* workspace-level threads (portfolio) */
const THREADS = [
  {
    id: "t-discount", title: "结算加优惠码", kind: "feature", lane: "needs",
    task: "给结算流程加优惠码：用户在结算页输入优惠码，实时校验并折扣。",
    lead: "claude", progress: { done: 1, total: 3 }, writes: ["api", "web", "mobile"],
    needs: 1, age: "2h",
  },
  {
    id: "t-gateway", title: "重构支付网关", kind: "refactor", lane: "working",
    task: "把支付网关从 v1 适配层迁移到 v2，去掉双写。",
    lead: "codex", progress: { done: 0, total: 2 }, writes: ["api", "infra"],
    needs: 0, age: "5h",
  },
  {
    id: "t-bug", title: "空购物车进结算崩溃", kind: "bugfix", lane: "review",
    task: "空购物车进入结算时前端崩溃 (#4821)。",
    lead: "codex", progress: { done: 1, total: 1 }, writes: ["web"],
    needs: 0, age: "40m",
  },
  {
    id: "t-docs", title: "优惠码 API 文档", kind: "feature", lane: "queued",
    task: "为新优惠码端点补 API 文档与示例。",
    lead: "claude", progress: { done: 0, total: 1 }, writes: ["docs"],
    needs: 0, age: "12m",
  },
  {
    id: "t-tokens", title: "暗色令牌对齐 AA", kind: "feature", lane: "delivered",
    task: "审计设计令牌在暗色下的对比度，统一到 AA。",
    lead: "claude", progress: { done: 2, total: 2 }, writes: ["tokens"],
    needs: 0, age: "1d",
  },
];

/* needs-you exceptions, aggregated across the workspace */
const NEEDS = [
  {
    id: "n1", kind: "permission", tool: "codex", thread: "结算加优惠码", direction: "Web 结算接入优惠码",
    age: "3m", title: "Codex 请求执行命令",
    detail: "pnpm dlx prisma migrate dev --name discount", reason: "应用优惠码表结构变更",
  },
  {
    id: "n2", kind: "escalation", tool: "opencode", thread: "重构支付网关", direction: "网关 v2 迁移",
    age: "14m", title: "Worker 主动升级：设计冲突",
    detail: "v2 网关不支持双写回滚，与现有审计要求冲突——需要你定夺保留哪条路径。",
  },
  {
    id: "n3", kind: "conflict", tool: "—", thread: "结算加优惠码 ↔ 重构支付网关", direction: "api",
    age: "1h", title: "硬冲突：两分支都改了 order.go",
    detail: "ws/checkout/discount/api 与 ws/gateway/v2/api 在 internal/order/order.go 冲突。",
  },
];

/* lead control-tower conversation stream */
const LEAD_STREAM = [
  { role: "user", text: "给结算流程加优惠码：用户在结算页输入优惠码，实时校验并折扣。" },
  { role: "lead", kind: "classify", text: "已归类为 **feature**。跨仓需求，先读仓库地图、划定 scope。" },
  { role: "lead", kind: "scope", title: "我推断的 scope" },
  { role: "user", text: "可以，mobile 也要做。" },
  { role: "lead", kind: "dispatch", title: "已派发 3 个子任务" },
  { role: "lead", kind: "contract", text: "**api** 已发布 `DiscountResult v1` 契约 → 解锁 web / mobile 两个下游子任务。" },
  { role: "lead", kind: "escalate", title: "1 件需要你" },
];

/* the scope proposal (the wow) */
const SCOPE = {
  task: "给结算流程加优惠码",
  inferred: [
    { repo: "api",    role: "write", reason: "优惠码校验逻辑与契约的归属服务", dir: "优惠码契约 + 校验端点", tool: "claude", order: 1 },
    { repo: "web",    role: "write", reason: "结算页 UI 接入优惠码输入与校验", dir: "Web 结算接入", tool: "codex", order: 2 },
    { repo: "mobile", role: "write", reason: "原生结算流程同样需要优惠码", dir: "移动端结算接入", tool: "opencode", order: 2 },
    { repo: "tokens", role: "read",  reason: "复用输入框 / 徽标令牌，不修改", dir: null, tool: null, order: null },
    { repo: "docs",   role: "read",  reason: "参考现有结算文档措辞", dir: null, tool: null, order: null },
    { repo: "infra",  role: "none",  reason: "不涉及部署或环境变更", dir: null, tool: null, order: null },
  ],
};

/* bus timeline (thread coordination) */
const BUS = [
  { from: "api", to: "all", kind: "contract", text: "published DiscountResult v1 { valid, amountOff, code }", age: "22m" },
  { from: "web", to: "api", kind: "ask", text: "amountOff 是分还是元？", age: "20m" },
  { from: "api", to: "web", kind: "reply", text: "整数分 (cents)。", age: "19m" },
  { from: "mobile", to: "all", kind: "status", text: "接入校验调用，跑通 9/22 测试", age: "6m" },
];

/* a session's chat timeline transcript */
const TERM = [
  { t: "sys", text: "claude · ws/checkout/discount/api · DiscountResult 契约子任务" },
  { t: "user", text: "实现 POST /checkout/discount，按 brief 的 acceptance 跑测试。" },
  { t: "asst", text: "已新增 handler 与校验逻辑，发布 DiscountResult 契约。运行验收：" },
  { t: "tool", text: "go test ./internal/checkout/... → ok  42 passed" },
  { t: "tool", text: "buf lint && buf breaking → no breaking changes" },
  { t: "asst", text: "全部通过。已开 PR #1284，并经 bus 向 web / mobile 广播契约。" },
];

const DIFF = [
  { file: "internal/checkout/discount.go", add: 96, del: 4, hunks: [
    { h: "@@ func (s *Service) ApplyDiscount", lines: [
      ["ctx","func (s *Service) ApplyDiscount(ctx context.Context, code string) (*DiscountResult, error) {"],
      ["add","  code = strings.ToUpper(strings.TrimSpace(code))"],
      ["add","  rule, err := s.repo.RuleByCode(ctx, code)"],
      ["add","  if err != nil { return &DiscountResult{Valid: false}, nil }"],
      ["del","  // TODO: discount validation"],
      ["add","  return rule.Evaluate(ctx, s.clock.Now()), nil"],
      ["ctx","}"],
    ]},
  ]},
  { file: "proto/checkout/v1/discount.proto", add: 41, del: 0, hunks: [
    { h: "@@ message DiscountResult", lines: [
      ["add","message DiscountResult {"],
      ["add","  bool valid = 1;"],
      ["add","  int64 amount_off = 2; // cents"],
      ["add","  string code = 3;"],
      ["add","}"],
    ]},
  ]},
];

Object.assign(window, {
  TOOLS, REPOS, repo, DIRECTIONS, THREADS, NEEDS,
  LEAD_STREAM, SCOPE, BUS, TERM, DIFF,
});
