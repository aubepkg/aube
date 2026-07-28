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
  cargoToml: 'Cargo.toml',
  cargoConfig: '.cargo/config.toml',
  npmrc: 'docs/.npmrc',
  workspaceYaml: 'docs/pnpm-workspace.yaml',
  tazeConfig: 'docs/taze.config.mts',
  // The repo deliberately ships NO rust-toolchain.toml (#1039: a repo-root
  // toolchain file outranks `rustup default`, redirected the MSRV CI legs,
  // and PPA builds must ride what Launchpad ships). The path stays declared
  // for the prebake parity check, which no-ops while the file is absent;
  // the nightly the cargo soak needs is requested per-invocation instead
  // (`cargo +nightly update` in update-deps.mts).
  toolchainToml: 'rust-toolchain.toml',
  renovateJson: '.github/renovate.json',
}

// The Ubuntu PPA builds aube on Launchpad, whose rustc is whatever the
// distro ships — bumping the workspace MSRV past it broke the source build
// and forced the #1039 revert of the first soak landing. This is the
// tracked ceiling: `mise run soak` fails any Cargo.toml rust-version above
// it. Raise it ONLY after confirming Launchpad's rustc moved
// (https://launchpad.net/ubuntu/+source/rustc).
// checked: 2026-07-28 (Ubuntu Resolute ships rustc 1.93.1)
export const PPA_RUST_CEILING: string | null = '1.93'

// The directory holding the npm package the soak governs (taze runs here,
// the repo's installer refreshes this package's lockfile).
export const NPM_PKG_DIR = path.join(REPO_ROOT, 'docs')

// Lockfile refreshers tried in order after taze rewrites package.json.
export const NPM_INSTALLERS: string[][] = [
  [path.join(REPO_ROOT, 'target/debug/aube'), 'install'],
  ['aube', 'install'],
]

// rustup's cargo shim — the only cargo that understands `+nightly`, and so
// the only one whose `cargo update` can honor the [unstable]
// min-publish-age soak (see .cargo/config.toml).
// CARGO_HOME-aware: rustup installs its shims under $CARGO_HOME/bin.
const CARGO_HOME = process.env.CARGO_HOME || path.join(os.homedir(), '.cargo')
export const RUSTUP_CARGO = path.join(
  CARGO_HOME,
  'bin',
  process.platform === 'win32' ? 'cargo.exe' : 'cargo',
)

// Pinned external tool manifest + the local tool rack it installs into:
// exact versions under rack/<tool>/<version>/, flat PATH handles in bin/.
export const EXTERNAL_TOOLS_JSON = path.join(REPO_ROOT, 'external-tools.json')

// CI agent image that pre-bakes the pinned sfw (null when the repo has no
// such image). The image builder's context is .buildkite/ alone, so the
// Dockerfile can't COPY the tracked pin sources — it embeds copies, and
// external-tools.mts --check asserts they haven't drifted.
export const DOCKER_PREBAKE: string | null = '.buildkite/linux-agent.Dockerfile'

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
