#!/usr/bin/env node
/**
 * Prepare the desktop shell's bundled runtime under .staging/runtime:
 *
 *   runtime/bin/node   portable Node binary (downloaded once, verified)
 *   runtime/dsh/       the dsh CLI closure (pnpm deploy of apps/cli with the
 *                      hoisted linker, completed with the workspace packages
 *                      pnpm deploy omits — vendored Cordis family and missing
 *                      @deepseek-ai peers — and symlinks materialized)
 *
 * The closure lives on the real filesystem inside the .app, so the harness's
 * profiles/node_modules heal mechanism (which symlinks the CLI's dependency
 * closure for out-of-tree profile plugins) resolves normally — unlike a pkg
 * SEA single executable, whose virtual /snapshot paths cannot back real
 * symlinks. Web-profile updates are picked up by re-running this script and
 * re-bundling, since the closure embeds the current workspace artifacts.
 *
 * Usage: node scripts/build-runtime.mjs  (tauri's beforeBuildCommand runs it
 * with src-tauri as cwd; all paths here anchor to the repo root).
 */

import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { cpSync, createWriteStream, existsSync, mkdirSync, readFileSync, rmSync } from 'node:fs'
import { pipeline } from 'node:stream/promises'
import { join, resolve } from 'node:path'

// Repo root = two parents above apps/desktop/scripts.
const repo = resolve(import.meta.dirname, '..', '..', '..')
const desktop = join(repo, 'apps', 'desktop')
const staging = join(desktop, '.staging')
const runtime = join(staging, 'runtime')

/** Node version the bundled runtime pins; must match what the workspace builds against. */
const NODE_VERSION = 'v24.19.0'
/** Mirror URL for the portable Node tarball (nodejs.org is slow on some networks). */
const NODE_TARBALL_URL = `https://mirrors.huaweicloud.com/nodejs/${NODE_VERSION}/node-${NODE_VERSION}-darwin-arm64.tar.gz`
const NODE_TARBALL_CACHE = join(staging, `node-${NODE_VERSION}-darwin-arm64.tar.gz`)
/** Expected SHA256 of the official tarball (verified against nodejs.org SHASUMS256.txt). */
const NODE_TARBALL_SHA = '8294b7aa9b03997481c06babf1e8b270c859358f27da57a11509afe537ac381d'

/** Run a pnpm workspace command from the repo root, inheriting stdio. */
function pnpm(args, cwd = repo) {
  execFileSync('pnpm', args, { cwd, stdio: 'inherit', env: { ...process.env, CI: 'true' } })
}

/** Deploy the CLI closure with the hoisted linker, then complete it. */
function buildDshClosure() {
  const dsh = join(runtime, 'dsh')
  rmSync(dsh, { recursive: true, force: true })
  mkdirSync(dsh, { recursive: true })
  pnpm([
    '--filter', '@deepseek-ai/dsh', 'deploy', '--legacy', '--prod',
    '--config.node-linker=hoisted',
    '--config.auto-install-peers=false',
    '--config.link-workspace-packages=true',
    dsh,
  ])
  // pnpm deploy omits the vendored Cordis family (link: overrides) and some
  // workspace peers; complete-closure copies them in and materializes links.
  execFileSync('node', [join(desktop, 'scripts', 'complete-closure.mjs'), dsh], {
    cwd: repo,
    stdio: 'inherit',
  })
  console.log(`build-runtime: dsh closure ready (${dsh})`)
}

/** Download a URL to a file, streaming. */
async function download(url, destination) {
  const response = await fetch(url)
  if (!response.ok || !response.body) throw new Error(`build-runtime: download failed (${response.status}) for ${url}`)
  await pipeline(response.body, createWriteStream(destination))
}

/** Ensure the portable Node binary exists under runtime/bin. */
async function ensureNode() {
  const nodeBin = join(runtime, 'runtime', 'bin', 'node')
  if (existsSync(nodeBin)) return
  if (!existsSync(NODE_TARBALL_CACHE)) {
    console.log(`build-runtime: downloading ${NODE_TARBALL_URL}`)
    await download(NODE_TARBALL_URL, NODE_TARBALL_CACHE)
  }
  const actual = createHash('sha256').update(readFileSync(NODE_TARBALL_CACHE)).digest('hex')
  if (actual !== NODE_TARBALL_SHA) {
    throw new Error(`build-runtime: node tarball sha mismatch (got ${actual.slice(0, 16)}…)`)
  }
  const extractDir = join(staging, `node-${NODE_VERSION}`)
  rmSync(extractDir, { recursive: true, force: true })
  mkdirSync(extractDir, { recursive: true })
  execFileSync('tar', ['-xzf', NODE_TARBALL_CACHE, '-C', extractDir, `node-${NODE_VERSION}/bin/node`], { stdio: 'inherit' })
  mkdirSync(join(runtime, 'runtime', 'bin'), { recursive: true })
  cpSync(join(extractDir, `node-${NODE_VERSION}`, 'bin', 'node'), nodeBin)
  rmSync(extractDir, { recursive: true, force: true })
  console.log(`build-runtime: portable node ready (${nodeBin})`)
}

async function main() {
  mkdirSync(runtime, { recursive: true })
  await ensureNode()
  buildDshClosure()
  console.log('build-runtime: runtime bundle ready at', runtime)
}

await main()
