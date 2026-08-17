#!/usr/bin/env node
/**
 * Complete a pnpm-deployed closure with the workspace packages pnpm deploy
 * omits: the vendored Cordis family (linked through `link:` overrides, which
 * deploy does not materialize) and any `@deepseek-ai/*` peer or direct
 * dependency of packages already in the closure that the deploy did not pull
 * in. Copies real files (dereferenced), so the staged closure stays
 * symlink-free for packaging.
 *
 * Usage: node scripts/complete-closure.mjs <closure-node_modules> [repo-root]
 * (repo-root defaults to the repository root above this script).
 */

import { cp, lstat, mkdir, readdir, readFile, realpath, rm } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { dirname, join, resolve, sep } from 'node:path'

const repo = resolve(import.meta.dirname, '..', '..', '..')
const closure = resolve(process.argv[2] ?? '')
if (closure === '') {
  console.error('complete-closure: usage: node scripts/complete-closure.mjs <closure-node_modules> [repo-root]')
  process.exit(1)
}

/** Vendored packages live under vendor/<dir>; everything else under packages/<group>/<name>. */
const VENDOR_DIRS = new Map([
  ['cosmokit', 'cosmokit'],
  ['schemastery', 'schemastery'],
  ['cordis', 'cordis'],
  ['cordis-plugin-group', 'group'],
  ['cordis-plugin-hmr', 'hmr'],
  ['cordis-plugin-include', 'include'],
  ['cordis-plugin-loader', 'loader'],
  ['cordis-plugin-logger-console', 'logger-console'],
  ['cordis-plugin-timer', 'timer'],
])

/** Resolve the repo directory whose package.json declares `name`. */
async function packageDirOf(name) {
  const basename = name.replace(/^@deepseek-ai\//, '')
  const vendor = VENDOR_DIRS.get(basename)
  if (vendor !== undefined) {
    const candidate = join(repo, 'vendor', vendor)
    if (existsSync(join(candidate, 'package.json'))) return candidate
  }
  // Index packages/<group>/*/package.json by declared name (the directory
  // basename is not always the package name, e.g. packages/fs/fs).
  for (const group of await readdir(join(repo, 'packages'))) {
    const groupDir = join(repo, 'packages', group)
    let entries
    try {
      entries = await readdir(groupDir)
    } catch {
      continue
    }
    for (const entry of entries) {
      const manifestPath = join(groupDir, entry, 'package.json')
      if (!existsSync(manifestPath)) continue
      try {
        const manifest = JSON.parse(await readFile(manifestPath, 'utf8'))
        if (manifest.name === name) return join(groupDir, entry)
      } catch {
        // Unreadable manifest; not the package we want.
      }
    }
  }
  return undefined
}

/** All @deepseek-ai package names declared anywhere in a package.json. */
function deepseekDeps(manifest) {
  const names = new Set()
  for (const field of ['dependencies', 'peerDependencies', 'optionalDependencies']) {
    for (const name of Object.keys(manifest[field] ?? {})) {
      if (name.startsWith('@deepseek-ai/')) names.add(name)
    }
  }
  return names
}

/** Read every package.json under the closure and collect their @deepseek-ai deps. */
async function collectRequired(closureModules) {
  const required = new Set()
  const queue = [closureModules]
  const seen = new Set()
  while (queue.length > 0) {
    const dir = queue.shift()
    if (seen.has(dir)) continue
    seen.add(dir)
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name)
      if (entry.isDirectory()) {
        if (entry.name === '.bin' || entry.name.startsWith('.')) continue
        if (existsSync(join(path, 'package.json'))) {
          const manifest = JSON.parse(await readFile(join(path, 'package.json'), 'utf8'))
          for (const name of deepseekDeps(manifest)) required.add(name)
          queue.push(path)
        } else {
          queue.push(path)
        }
      }
    }
  }
  return required
}

/** Copy one package directory into the closure, dereferencing links. */
async function copyPackage(source, destination) {
  const nested = join(source, 'node_modules')
  await mkdir(dirname(destination), { recursive: true })
  await cp(source, destination, {
    recursive: true,
    dereference: true,
    filter: path => path !== nested && !path.startsWith(nested + sep),
  })
}

/** Replace symlinks under the closure with their dereferenced files. */
async function materializeLinks(closureModules) {
  const queue = [closureModules]
  while (queue.length > 0) {
    const dir = queue.shift()
    for (const entry of await readdir(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name)
      if (entry.isSymbolicLink()) {
        if (entry.name === '.bin') {
          await rm(path, { recursive: true, force: true })
          continue
        }
        const source = await realpath(path)
        await rm(path, { recursive: true, force: true })
        await copyPackage(source, path)
      } else if (entry.isDirectory()) {
        if (entry.name.startsWith('.')) continue
        queue.push(path)
      }
    }
  }
}

const closureModules = join(closure, 'node_modules')
const required = await collectRequired(closureModules)
let rounds = 0
let added = 0
do {
  rounds += 1
  added = 0
  const missing = [...required].filter(name => !existsSync(join(closureModules, '@deepseek-ai', name.replace(/^@deepseek-ai\//, ''))))
  for (const name of missing) {
    const source = await packageDirOf(name)
    if (source === undefined) {
      console.warn(`complete-closure: no repo package for ${name}; skipping`)
      continue
    }
    const destination = join(closureModules, name)
    await copyPackage(source, destination)
    const manifest = JSON.parse(await readFile(join(destination, 'package.json'), 'utf8'))
    for (const dep of deepseekDeps(manifest)) required.add(dep)
    added += 1
    console.log(`complete-closure: copied ${name}`)
  }
} while (added > 0 && rounds < 10)

await materializeLinks(closureModules)
console.log(`complete-closure: done after ${rounds} round(s); closure at ${closure}`)
