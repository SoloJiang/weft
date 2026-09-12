# weft-codex 本地发行包

这个发行包只依赖官方 Codex Desktop，不依赖原 Weft 客户端或原 Weft 数据。

## 安装为全局 CLI

```sh
./install.sh
weft-codex doctor
weft-codex
```

默认安装到 `~/.local`：稳定入口为 `~/.local/bin/weft-codex`，实际 runtime
保存在 `~/.local/share/weft-codex/releases/`，升级时通过 `current` 原子切换。
可通过 `WEFT_CODEX_PREFIX=/absolute/path ./install.sh` 更改安装前缀。

## 直接运行

运行 `bin/weft-codex doctor` 检查环境，再运行 `bin/weft-codex`。
无参数即启动完整 Host。Host 自身没有窗口、Dock 图标或聊天界面；它启动并接入的
是 `/Applications/ChatGPT.app` 里的官方 Codex，默认进入 Weft mode。第一次使用
专用 profile 时可能需要登录。

官方应用必须位于 `/Applications/ChatGPT.app`。若安装在其他位置，使用
`--app-path=/absolute/path/ChatGPT.app`。

## 数据与回退

新数据默认保存在 `~/.weft-codex`，不会读取或修改 `~/.weft`。退出 Host 后，
它会移除注入、恢复 CSP 并回收自己启动的 Codex 与 weftd；随后直接打开官方
ChatGPT/Codex 即为原生状态。也可以使用 `bin/weft-codex --safe-mode` 启动官方
体验（Host 不加载 Weft surface）。

## 安全边界

CDP 和 weftd 都只监听 loopback。只有 `connect-src` / `script-src` 拦住
loopback 时，Host 才在它创建的专用 profile 中临时启用 CSP compatibility
mode，并在退出时恢复。不为 `frame-src` 开 bypass。

Host 不是第二个 App，也不复制、修改或重签官方 Codex。若未来通过正式渠道分发
Host 二进制，仍需独立完成 Developer ID 签名和 notarization。

## Product skills

Runtime packages ship Codex skills under `share/weft-codex/skills/`.
`./install.sh`, `weft-codex doctor`, and normal `weft-codex` starts sync those
skills into `$CODEX_HOME/skills` (default `~/.codex/skills`). Managed copies are
marked with `.weft-managed` and refresh only when the package skill `version`
changes (or when you pass `--force`).

```sh
weft-codex install-skills          # explicit refresh
weft-codex install-skills --force  # overwrite a local non-managed fork
```

