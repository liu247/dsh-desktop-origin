# @deepseek-ai/dsh-desktop

[English](README.md) | 中文

DeepSeek Harness Web GUI 的桌面壳，基于 Tauri v2 构建。壳本身是一个薄监督层：它启动 harness 自己的 `dsh --profile web` 服务进程（该进程拥有全部插件、会话与 HTTP/WebSocket 接口），等待 web 运行时打印的就绪行，然后让原生窗口导航到该 URL。因此窗口渲染的就是浏览器版所提供的同一个 SPA 与插件 registry——host 插件与 client 插件、会话、API 网关全部原样运行，网页版任何更新（dist 重建、新增 client bundle）都会在下次启动时反映到桌面版。

## 部署与编译打包

本模块是官方 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) 仓库的 workspace 成员（位于其 `apps/desktop` 目录）。它**不是独立项目**：壳会启动 harness 自己的 `dsh web` 服务，release 包内嵌 workspace 的构建产物，因此必须在完整 checkout 内构建。拉取官方 `master` 到本地、保留（或放入）这个 `apps/desktop` 目录即可**无缝编译**——模块跟随官方 workspace API，不 fork 官方代码。

环境要求：macOS（arm64 或 x64）、Node ≥ 22、pnpm ≥ 10、Rust 工具链（仅 Tauri 壳需要）。

```sh
# 1. Get the official checkout (or update an existing one)
git clone https://github.com/deepseek-ai/deepseek-harness.git
cd deepseek-harness
#    (this module lives at apps/desktop/ in the checkout)

# 2. Install workspace dependencies
pnpm install

# 3. Build the web frontend (the SPA the desktop renders)
pnpm --filter @deepseek-ai/dsh-web-frontend build

# 4. Run the desktop shell against the checkout's source (dev mode)
pnpm --filter @deepseek-ai/dsh-desktop dev

# 5. Package the release .app / .dmg (runs scripts/build-runtime.mjs first:
#    stages the bundled Node + dsh CLI closure, then bundles it into the app)
pnpm --filter @deepseek-ai/dsh-desktop build
```

产物位于 `apps/desktop/src-tauri/target/release/bundle/`：
`macos/DeepSeek Harness.app` 与 `dmg/DeepSeek Harness_<version>_aarch64.dmg`。`.dmg` 即分发包，把 `.app` 拖入 Applications 即可。

壳固定使用桌面端口（31080），使 WebView 保持单一稳定 origin、localStorage 跨启动持久。release 包自包含（便携 Node + dsh CLI 闭包 + SPA dist 全部内嵌），打包出的 `.app` 可在没有 checkout 的机器上运行——用户 profile（`~/.dsh`）首次启动时创建并维护，`dsh plugin install` 可像浏览器版一样安装第三方插件。

## 命令

```sh
pnpm --filter @deepseek-ai/dsh-desktop dev    # tauri dev: run against the checkout's CLI from source
pnpm --filter @deepseek-ai/dsh-desktop build  # tauri build: bundle the release app (.app/.dmg on macOS)
pnpm --filter @deepseek-ai/dsh-desktop icon   # regenerate src-tauri/icons from a source image
```

`tauri dev` 需要 PATH 中有 Node 运行时以及本 workspace checkout（开发模式的服务命令是 `node --import tsx/esm apps/cli/src/bin.ts web --port 31080`，因此当前构建好的 client bundle 与 web dist 与 `dsh web` 服务的内容完全一致）。

## 架构

```
┌──────────────────────────────────────────────┐
│ dsh-desktop (Tauri, Rust)                     │
│  ├─ service: spawn dsh --profile web --port 31080 │ ← every plugin/session/API
│  ├─ window:  navigate to the printed URL      │ ← the browser edition's SPA
│  ├─ tray:    show/hide, quit                  │
│  └─ lifecycle: SIGTERM→SIGKILL, restart on    │
│     unexpected exit                           │
└──────────────────────────────────────────────┘
```

- **就绪**：web 运行时打印 `dsh web: http://127.0.0.1:31080`。壳解析该行后导航。桌面端口固定为 31080（与浏览器版的 3080 分开），使 WebView 保持单一稳定 origin，其 localStorage（插件偏好、皮肤、任务看板、面板折叠状态）跨启动持久——随机端口会让每次启动的 origin 都不同、丢失全部存储偏好。
- **服务生命周期**：`src-tauri/src/service.rs` 持有子进程——stdout 排空、优雅停止（SIGTERM，3 秒后 SIGKILL）、退出码上报。
- **窗口**：关闭即隐藏到托盘；托盘菜单负责显示与退出。退出前先停止服务进程。
- **重启**：服务异常退出会重启一次（1 秒退避）并重新导航窗口。

## 打包（release 构建）

`tauri build` 会先运行 `scripts/build-runtime.mjs`，在 `.staging/runtime` 下准备捆绑运行时：

```
.staging/runtime/
  runtime/bin/node        # portable Node 24 binary (downloaded once, sha-verified)
  dsh/                    # the dsh CLI closure: pnpm deploy of apps/cli with the
                          #   hoisted linker, completed with the workspace packages
                          #   deploy omits (vendored Cordis family + missing
                          #   @deepseek-ai peers) and links materialized
```

闭包通过 `bundle.resources` 嵌入 .app。release 模式的服务命令运行
`<resources>/runtime/bin/node <resources>/dsh/lib/bin.js web --port 31080`。闭包位于真实文件系统，因此 harness 的 `profiles/node_modules` heal 机制（为 profile 外插件符号链接 CLI 依赖闭包）能正常解析——曾尝试 pkg SEA 单文件可执行，但其虚拟 `/snapshot` 路径无法作为真实符号链接目标，会破坏 profile 插件解析。

由于闭包内嵌当前 workspace 的产物（`@deepseek-ai/dsh-web-frontend` 的 SPA dist 与全部 client 插件 bundle），网页版更新只需重跑 `tauri build`：打包出的 `.app`/`.dmg` 即渲染新 GUI、新 host 插件与新 client 插件。开发模式始终运行 checkout 源码，因此 `tauri dev` 无需打包步骤即可反映更新。

## 模型体验

无。壳只托管服务并渲染 GUI；其任何行为都不会进入模型请求。所有模型可见行为都属于它所启动的 harness 包。

#### KV Cache 影响

无；该包既不组装也不发送提供方请求。
