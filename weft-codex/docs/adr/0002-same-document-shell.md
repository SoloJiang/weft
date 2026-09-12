# ADR 0002：同文档壳，不做产品 OOPIF

- 状态：**Accepted**（2026-08-16）
- 相关：[`PRODUCT.md`](../../PRODUCT.md) Product Form、
  [`2026-08-16-weft-third-mode-design.md`](../specs/2026-08-16-weft-third-mode-design.md)
- 影响：`ui/` 的 surface / Host Context、`launcher/` 的 iframe agent、weftd CORS

## 背景

08-08 把 React UI 做成三个跨源 iframe（sidebar / workspace / modal），再用
`weft:host-context` 把 token、locale、dialog 状态桥过去。那是为了同一份构建
既能嵌进 Desktop，又能在浏览器里当一等降级面。

产品形态已经锁成「Desktop 第三种模式，没有浏览器降级」。iframe 隔离不再服务
任何用户可见目标，只留下黑底、`frame-src` bypass、BroadcastChannel 和第三棵
dialog 树。

## 决策

**产品壳改为同文档一份 React 树，挂在三个 open shadow root 上。**
不再有产品 OOPIF，不再有跨 frame 协议，不再有 additive / 浏览器两条假降级。

具体锁：

1. `mountWeft({ sidebar, main, overlay, host })` 是唯一产品入口。
2. 侧栏与主区用 portal；dialog 只进 overlay shadow。
3. 主题靠 CSS 变量继承；半径读成 px 再写入，避免 16px / 13px 根字号各解各的。
4. API 走 `host.weftdOrigin` + weftd CORS。不为 `frame-src` 开 CSP bypass。
5. 探针失败则不能进入 Weft。调试日志可以提 `additive`，用户看不到那一档。

Vite 顶层页只作开发预览。

## 依据

- 产品未上线，壳层假设可以推翻，不必迁就已发布的 iframe 路径。
- 社区同类（Explodex、Codex++）是 CDP + 同页挂载，没有浏览器降级产品。
- 真机已经付出过 iframe 税：build 6119 / 6321 的 `frame-src`、透明 iframe 的
  `color-scheme` 纯黑底、token 必须信封转发。
- ADR 0001 已要求拿掉从未填充的 `projectId`。同文档 Host 对象比
  `HostContextV1` 更干净。

## 后果

- `surface-channel.ts`、`ModalApp`、iframe handshake、dialog 代持全部删除。
- weftd 必须承认 Desktop 宿主 origin。编排与 schema 不动。
- 兼容矩阵里历史 `additive` 行仍是当时实测，不再是产品档。
- 官方若出模式级 API，只换 launcher 壳，不换 weftd。

同文档壳落地之后，#58 跟踪的「host-context 握手超时、重试即成功」不再存在。
launcher 等的是 `mountWeft` 的 `uiReady`；CSP reload 后预算是 15s / 8s，失败
文案区分「宿主壳没出来」和「Weft UI 没挂上」。真机若还偶发，另开 shadow
挂载问题，不要按 iframe 握手去改。

## 翻案条件

只有在官方提供稳定的 `ui.sidebar` / 模式 contribution，或同文档挂载被发行版
CSP 全面禁止且 bypass 也无法加载 library 时，才重新评估壳。即使翻案，也不把
浏览器页做成产品面。
