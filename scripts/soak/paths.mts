/**
 * @file 1 path, 1 reference — every filesystem location the soak +
 *   external-tools scripts touch is declared here exactly once. Scripts
 *   import from this module instead of re-deriving paths, so a surface can
 *   move (or differ between repos carrying these scripts) with a one-line
 *   change.
 */

import os from 'node:os'
import path from 'node:path'
import process from 'node:process'
import { fileURLToPath } from 'node:url'

export const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')

// Soak surfaces (repo-relative). aube keeps its npm surfaces in docs/ —
// that's where the package.json lives and where aube itself reads workspace
// yaml + npmrc; a ROOT pnpm-workspace.yaml would re-anchor the docs install
// at the repo root.
export const SURFACES = {
  cargoConfig: '.cargo/config.toml',
  npmrc: 'docs/.npmrc',
  workspaceYaml: 'docs/pnpm-workspace.yaml',
  tazeConfig: 'docs/taze.config.mts',
  toolchainToml: 'rust-toolchain.toml',
  renovateJson: '.github/renovate.json',
}

// The directory holding the npm package the soak governs (taze runs here,
// the repo's installer refreshes this package's lockfile).
export const NPM_PKG_DIR = path.join(REPO_ROOT, 'docs')

// Lockfile refreshers tried in order after taze rewrites package.json.
export const NPM_INSTALLERS: string[][] = [
  [path.join(REPO_ROOT, 'target/debug/aube'), 'install'],
  ['aube', 'install'],
]

// rustup's cargo shim — the only cargo that reads rust-toolchain.toml and
// therefore the only one whose `cargo update` honors the [unstable]
// min-publish-age soak.
export const RUSTUP_CARGO = path.join(os.homedir(), '.cargo/bin/cargo')

// Pinned external tool manifest + the local tool rack it installs into:
// exact versions under rack/<tool>/<version>/, flat PATH handles in bin/.
export const EXTERNAL_TOOLS_JSON = path.join(REPO_ROOT, 'external-tools.json')

// CI agent image that pre-bakes the pinned toolchain + sfw (null when the
// repo has no such image). The image builder's context is .buildkite/
// alone, so the Dockerfile can't COPY the tracked pin sources — it embeds
// copies, and external-tools.mts --check asserts they haven't drifted.
export const DOCKER_PREBAKE: string | null = '.buildkite/linux-agent.Dockerfile'

// .dockerignore managed by `untracked` (null when the repo has no
// repo-context docker builds — aube's agent image never copies the repo).
export const DOCKERIGNORE: string | null = null

const XDG_DATA_HOME = process.env.XDG_DATA_HOME || path.join(os.homedir(), '.local/share')
export const DEV_TOOLS_DIR = path.join(XDG_DATA_HOME, 'aube/dev-tools')
export const RACK_DIR = path.join(DEV_TOOLS_DIR, 'rack')
export const BIN_DIR = path.join(DEV_TOOLS_DIR, 'bin')


// Candidates (tried in order) for installing an extracted external tool's
// runtime deps — the repo's own package manager first, of course.
export const PM_DEP_INSTALLERS: string[][] = [
  [path.join(REPO_ROOT, 'target/debug/aube'), 'install', '--prod'],
  ['aube', 'install', '--prod'],
  ['pnpm', 'install', '--prod', '--ignore-scripts'],
  ['npm', 'install', '--omit=dev', '--ignore-scripts', '--no-audit', '--no-fund'],
]
