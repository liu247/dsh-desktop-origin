# @deepseek-ai/dsh-desktop

[中文](README.zh.md) | English

Desktop shell for the DeepSeek Harness Web GUI, built with Tauri v2. The shell
is a thin supervisor: it spawns the harness's own `dsh --profile web` service
(which owns every plugin, session, and the HTTP/WebSocket surface), waits for
the readiness line the web runtime prints, and navigates a native window to
that URL. The window therefore renders the exact SPA and plugin registry the
browser edition serves — host and client plugins, sessions, and the API
gateway run unchanged, and any web-edition update (dist rebuild, new client
bundle) shows up in the desktop shell on the next launch.

## Deploy & build

This module is a workspace member of the official
[deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) repository
(it lives at `apps/desktop` there). It is **not** a standalone project: the
shell boots the harness's own `dsh web` service and the release bundle embeds
the workspace's built artifacts, so it must be built inside a full checkout.
Pulling the official `master` to your machine and keeping (or dropping in)
this `apps/desktop` directory compiles seamlessly — the module tracks the
official workspace APIs and does not fork them.

Requirements: macOS (arm64 or x64), Node ≥ 22, pnpm ≥ 10, and a Rust toolchain
(needed only for the Tauri shell).

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

Artifacts land in `apps/desktop/src-tauri/target/release/bundle/`:
`macos/DeepSeek Harness.app` and `dmg/DeepSeek Harness_<version>_aarch64.dmg`.
The `.dmg` is the distributable; drag the `.app` into Applications.

The shell pins a fixed desktop port (31080) so the WebView keeps one stable
origin and its localStorage survives restarts. The release bundle is
self-contained (portable Node + the dsh CLI closure + the SPA dist are all
embedded), so the packaged `.app` runs on a machine without the checkout —
the user profile (`~/.dsh`) is created and maintained on first launch, and
`dsh plugin install` adds third-party plugins as in the browser edition.

## Commands

```sh
pnpm --filter @deepseek-ai/dsh-desktop dev    # tauri dev: run against the checkout's CLI from source
pnpm --filter @deepseek-ai/dsh-desktop build  # tauri build: bundle the release app (.app/.dmg on macOS)
pnpm --filter @deepseek-ai/dsh-desktop icon   # regenerate src-tauri/icons from a source image
```

`tauri dev` needs a Node runtime on PATH and the workspace checkout (the dev
service command is `node --import tsx/esm apps/cli/src/bin.ts web --port 31080`,
so the currently built client bundles and web dist are served exactly as `dsh
web` serves them).

## Architecture

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

- **Readiness**: the web runtime prints `dsh web: http://127.0.0.1:31080`.
  The shell parses that line, then navigates. The desktop port is fixed at
  31080 (separate from the browser edition's 3080) so the WebView keeps one
  stable origin and its localStorage — plugin prefs, skins, the task board,
  panel collapse state — survives restarts; a random port would create a
  fresh origin every launch and drop every stored preference.
- **Service lifecycle**: `src-tauri/src/service.rs` owns the child process —
  stdout drain, graceful stop (SIGTERM, SIGKILL after 3s), exit-code reporting.
- **Window**: closing hides to the tray; the tray menu shows and quits.
  Quitting stops the service before the process exits.
- **Restart**: an unexpected service exit restarts it once (1s backoff) and
  re-navigates the window.

## Packaging (release build)

`tauri build` runs `scripts/build-runtime.mjs` first, which stages the bundled
runtime under `.staging/runtime`:

```
.staging/runtime/
  runtime/bin/node        # portable Node 24 binary (downloaded once, sha-verified)
  dsh/                    # the dsh CLI closure: pnpm deploy of apps/cli with the
                          #   hoisted linker, completed with the workspace packages
                          #   deploy omits (vendored Cordis family + missing
                          #   @deepseek-ai peers) and links materialized
```

The closure is then embedded in the .app via `bundle.resources`. The shell's
release service command runs `<resources>/runtime/bin/node
<resources>/dsh/lib/bin.js web --port 31080`. The closure lives on the real
filesystem, so the harness's `profiles/node_modules` heal mechanism (which
symlinks the CLI's dependency closure for out-of-tree profile plugins)
resolves normally — a pkg SEA single executable was tried first but its
virtual `/snapshot` paths cannot back real symlinks, breaking profile plugin
resolution.

Because the closure embeds the current workspace artifacts (the SPA dist from
`@deepseek-ai/dsh-web-frontend` and every client plugin bundle), a web-edition
update is picked up by re-running `tauri build`: the packaged `.app`/`.dmg`
then renders the new GUI, host plugins, and client plugins. The dev mode
always runs the checkout's source, so `tauri dev` reflects updates without a
bundle step.

## Model Experience

None. The shell hosts the service and renders the GUI; nothing it does reaches
a model request. All model-visible behavior belongs to the harness packages it
spawns.

#### KV Cache Impact

None; the shell neither assembles nor sends provider requests.
