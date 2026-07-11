#!/usr/bin/env node
/**
 * @file Soaked dependency updater — every ecosystem bumps through the same
 *   cooldown:
 *
 *   - npm: taze (maturityPeriod = SOAK_DAYS via the taze config next to the
 *     package.json) rewrites ranges, then the repo's own installer refreshes
 *     the lockfile.
 *   - cargo: `cargo update` under the pinned nightly, where
 *     `.cargo/config.toml` min-publish-age enforces the same window
 *     (too-new crate versions are skipped unless already locked).
 *
 *   Usage: node scripts/soak/update-deps.mts [--npm|--cargo] [--dry-run]
 *   (no ecosystem flag = both)
 */

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')

// Per-repo knobs — the only block that differs between repos carrying this
// script (aube's npm package lives in docs/ and installs with aube itself).
const NPM_DIR = path.join(REPO_ROOT, 'docs')
const INSTALLERS = [
  [path.join(REPO_ROOT, 'target/debug/aube'), 'install'],
  ['aube', 'install'],
]

function run(cmd: string, args: string[], cwd: string): number {
  console.log(`[update-deps] ${cmd} ${args.join(' ')} (in ${path.relative(REPO_ROOT, cwd) || '.'})`)
  const res = spawnSync(cmd, args, { cwd, stdio: 'inherit' })
  return res.status ?? 1
}

function updateNpm(dryRun: boolean): number {
  const taze = path.join(NPM_DIR, 'node_modules/.bin/taze')
  if (!existsSync(taze)) {
    console.error(`[update-deps] taze not installed — run the installer in ${NPM_DIR} first`)
    return 1
  }
  const args = dryRun ? [] : ['--write']
  const status = run(taze, args, NPM_DIR)
  if (status !== 0 || dryRun) {
    return status
  }
  for (const [cmd, ...args] of INSTALLERS) {
    if (cmd.includes('/') && !existsSync(cmd)) {
      continue
    }
    return run(cmd!, args, NPM_DIR)
  }
  console.error('[update-deps] no installer found — refresh the lockfile manually')
  return 1
}

function updateCargo(dryRun: boolean): number {
  // The min-publish-age soak is an [unstable] cargo feature: only the
  // rust-toolchain.toml nightly honors it, and only rustup's cargo shim
  // reads rust-toolchain.toml. A non-rustup cargo (e.g. Homebrew stable)
  // would silently update WITHOUT the soak — refuse that.
  const rustupCargo = path.join(os.homedir(), '.cargo/bin/cargo')
  if (!existsSync(rustupCargo)) {
    console.error('[update-deps] rustup cargo shim not found — cargo update would bypass the min-publish-age soak')
    return 1
  }
  return run(rustupCargo, dryRun ? ['update', '--dry-run'] : ['update'], REPO_ROOT)
}

function main(argv: string[] = process.argv.slice(2)): number {
  const dryRun = argv.includes('--dry-run')
  const onlyNpm = argv.includes('--npm')
  const onlyCargo = argv.includes('--cargo')
  let status = 0
  if (!onlyCargo) {
    status ||= updateNpm(dryRun)
  }
  if (!onlyNpm) {
    status ||= updateCargo(dryRun)
  }
  return status
}

const isMain = process.argv[1] && import.meta.url === `file://${process.argv[1]}`
if (isMain) {
  process.exitCode = main()
}
