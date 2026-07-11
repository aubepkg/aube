#!/usr/bin/env node
/**
 * @file Pinned external security tooling — download, verify, shim.
 *   `external-tools.json` (repo root) pins every tool to an exact version
 *   with a sha512 SRI integrity per platform asset. This script is the only
 *   way those tools reach a machine: nothing here trusts "latest".
 *
 *   - `--check`            validate every pin (shape, SRI prefix, soak
 *                          annotations on any soakBypass) — CI gate, no network
 *   - `--install <name>`   download + SRI-verify + install into the local
 *                          tool rack (see paths.mts RACK_DIR) with a PATH
 *                          handle in BIN_DIR
 *   - `--install-all`      every installable pin
 *   - `--shims`            write sfw shims (npm/yarn/pnpm/pip/pip3/uv/cargo)
 *                          into BIN_DIR so installs route through the firewall
 *   - `--print-bin`        print BIN_DIR (for `>> $GITHUB_PATH` in CI)
 *
 *   `sfw` resolves to sfw-enterprise when SOCKET_API_KEY or SOCKET_API_TOKEN
 *   is set (either may be fed from a repo secret such as
 *   SOCKET_SECURITY_KEY), else sfw-free — free tier needs no key, so CI is
 *   firewalled from day one and upgrades itself when the secret lands.
 */

import { createHash } from 'node:crypto'
import { spawnSync } from 'node:child_process'
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs'
import path from 'node:path'
import process from 'node:process'

import { ANNOTATION_RE, SOAK_DAYS, addDaysIso } from './constants.mts'
import { BIN_DIR, EXTERNAL_TOOLS_JSON, RACK_DIR } from './paths.mts'

const SFW_ECOSYSTEMS = ['npm', 'yarn', 'pnpm', 'pip', 'pip3', 'uv', 'cargo']

interface PlatformPin {
  asset: string
  integrity: string
}

interface ToolPin {
  description?: string
  version?: string
  repository?: string
  release?: string
  binaryName?: string
  purl?: string
  integrity?: string
  platforms?: Record<string, PlatformPin>
  soakBypass?: { version: string; published: string; removable: string }
}

function loadTools(): Record<string, ToolPin> {
  return JSON.parse(readFileSync(EXTERNAL_TOOLS_JSON, 'utf8')).tools
}

function platformKey(): string {
  const osKey = { darwin: 'darwin', linux: 'linux', win32: 'win' }[process.platform]
  const archKey = { arm64: 'arm64', x64: 'x64' }[process.arch]
  if (!osKey || !archKey) {
    throw new Error(`unsupported platform ${process.platform}-${process.arch}`)
  }
  return `${osKey}-${archKey}`
}

function sriSha512(buf: Buffer): string {
  return `sha512-${createHash('sha512').update(buf).digest('base64')}`
}

export function checkPins(tools: Record<string, ToolPin>): string[] {
  const out: string[] = []
  for (const [name, pin] of Object.entries(tools)) {
    if (!pin.version && !pin.purl) {
      out.push(`${name}: no version or purl pin`)
    }
    const integrities = [
      ...(pin.integrity ? [pin.integrity] : []),
      ...Object.values(pin.platforms ?? {}).map(p => p.integrity),
    ]
    if (pin.release === 'asset' && integrities.length === 0) {
      out.push(`${name}: release asset without any integrity pin`)
    }
    for (const sri of integrities) {
      if (!/^sha512-[A-Za-z0-9+/]+={0,2}$/.test(sri)) {
        out.push(`${name}: integrity is not a sha512 SRI: ${sri}`)
      }
    }
    if (pin.soakBypass) {
      const { published, removable } = pin.soakBypass
      const expected = addDaysIso(published, SOAK_DAYS)
      if (removable !== expected) {
        out.push(`${name}: soakBypass removable ${removable}, wanted ${expected} (published + ${SOAK_DAYS}d)`)
      }
      if (!ANNOTATION_RE.test(`# published: ${published} | removable: ${removable}`)) {
        out.push(`${name}: soakBypass dates are not YYYY-MM-DD`)
      }
    }
  }
  return out
}

async function download(url: string, expectedSri: string): Promise<Buffer> {
  const headers: Record<string, string> = {}
  if (process.env.GITHUB_TOKEN) {
    headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`
  }
  // Fail fast on a stalled release/registry response instead of hanging
  // CI; 120s is generous for the largest pinned binary on a slow runner.
  const res = await fetch(url, {
    headers,
    redirect: 'follow',
    signal: AbortSignal.timeout(120_000),
  })
  if (!res.ok) {
    throw new Error(`download failed ${res.status} ${url}`)
  }
  const buf = Buffer.from(await res.arrayBuffer())
  const actual = sriSha512(buf)
  if (actual !== expectedSri) {
    throw new Error(`integrity mismatch for ${url}\n  expected ${expectedSri}\n  actual   ${actual}`)
  }
  return buf
}

function linkHandle(target: string, name: string): void {
  mkdirSync(BIN_DIR, { recursive: true })
  const handle = path.join(BIN_DIR, name)
  if (existsSync(handle)) {
    unlinkSync(handle)
  }
  symlinkSync(target, handle)
}

async function installAssetTool(name: string, pin: ToolPin): Promise<void> {
  const plat = pin.platforms?.[platformKey()]
  if (!plat) {
    throw new Error(`${name}: no pinned asset for ${platformKey()}`)
  }
  const repo = pin.repository!.replace(/^github:/, '')
  const url = `https://github.com/${repo}/releases/download/v${pin.version}/${plat.asset}`
  const binName = pin.binaryName ?? name
  const destDir = path.join(RACK_DIR, name, pin.version!)
  const destBin = path.join(destDir, binName)
  if (existsSync(destBin)) {
    linkHandle(destBin, binName)
    console.log(`[external-tools] ${name}@${pin.version} already installed`)
    return
  }
  console.log(`[external-tools] downloading ${name}@${pin.version} (${plat.asset})`)
  const buf = await download(url, plat.integrity)
  mkdirSync(destDir, { recursive: true })
  if (plat.asset.endsWith('.tar.gz')) {
    const archive = path.join(destDir, plat.asset)
    writeFileSync(archive, buf)
    const res = spawnSync('tar', ['-xzf', archive, '-C', destDir], { stdio: 'inherit' })
    rmSync(archive)
    if (res.status !== 0) {
      throw new Error(`${name}: tar extract failed`)
    }
  } else {
    writeFileSync(destBin, buf)
  }
  chmodSync(destBin, 0o755)
  linkHandle(destBin, binName)
  console.log(`[external-tools] installed ${name}@${pin.version} -> ${destBin}`)
}

function hasSocketToken(): boolean {
  return Boolean(process.env.SOCKET_API_KEY || process.env.SOCKET_API_TOKEN)
}

async function installTool(name: string, tools: Record<string, ToolPin>): Promise<void> {
  // `sfw` is a flavor pair: the enterprise binary when a Socket token is
  // present (repo secret), the keyless free tier otherwise.
  if (name === 'sfw') {
    name = hasSocketToken() ? 'sfw-enterprise' : 'sfw-free'
  }
  const pin = tools[name]
  if (!pin) {
    throw new Error(`unknown tool ${name} (see external-tools.json)`)
  }
  if (pin.release === 'asset') {
    await installAssetTool(name, pin)
    return
  }
  if (pin.purl) {
    // npm-packaged scanner (agentshield): verify the registry tarball
    // against the pinned SRI, then run via the extracted package.
    const m = /^pkg:npm\/(.+)@([^@]+)$/.exec(pin.purl)
    if (!m) {
      throw new Error(`${name}: unsupported purl ${pin.purl}`)
    }
    const [, pkg, version] = m
    const base = pkg!.split('/').pop()
    const url = `https://registry.npmjs.org/${pkg}/-/${base}-${version}.tgz`
    const destDir = path.join(RACK_DIR, name, version!)
    if (!existsSync(destDir)) {
      console.log(`[external-tools] downloading ${name}@${version} (npm)`)
      const buf = await download(url, pin.integrity!)
      mkdirSync(destDir, { recursive: true })
      const archive = path.join(destDir, 'package.tgz')
      writeFileSync(archive, buf)
      const res = spawnSync('tar', ['-xzf', archive, '-C', destDir], { stdio: 'inherit' })
      rmSync(archive)
      if (res.status !== 0) {
        throw new Error(`${name}: tar extract failed`)
      }
    }
    const pkgJson = JSON.parse(readFileSync(path.join(destDir, 'package/package.json'), 'utf8'))
    const binRel = typeof pkgJson.bin === 'string' ? pkgJson.bin : Object.values(pkgJson.bin ?? {})[0]
    if (binRel) {
      const wrapper = path.join(RACK_DIR, name, `${name}-wrapper`)
      writeFileSync(
        wrapper,
        `#!/usr/bin/env bash\nexec node '${path.join(destDir, 'package', binRel as string)}' "$@"\n`,
      )
      chmodSync(wrapper, 0o755)
      linkHandle(wrapper, name)
    }
    console.log(`[external-tools] installed ${name}@${version}`)
    return
  }
  if (pin.release === 'uv-project') {
    // Git-SHA-pinned python project; not auto-installed (needs uv).
    const repo = pin.repository!.replace(/^github:/, '')
    console.log(
      `[external-tools] ${name} is a uv project — run: uvx --from git+https://github.com/${repo}@${pin.version} ${name}`,
    )
    return
  }
  throw new Error(`${name}: no installable shape (release=${pin.release ?? 'none'})`)
}

/**
 * sfw shims: tiny wrappers named after each package manager that route the
 * real invocation through the firewall. A sentinel env var breaks the
 * recursion when sfw itself re-invokes the tool; the real binary is found
 * by stripping the rack's bin dir out of PATH.
 */
function writeShims(): void {
  mkdirSync(BIN_DIR, { recursive: true })
  for (const cmd of SFW_ECOSYSTEMS) {
    const sentinel = `SFW_SHIM_ACTIVE_${cmd.replace(/[^A-Za-z0-9]/g, '_').toUpperCase()}`
    const body = `#!/usr/bin/env bash
# sfw shim for ${cmd} — managed by scripts/soak/external-tools.mts --shims
set -euo pipefail
CLEAN_PATH=$(printf '%s' "$PATH" | tr ':' '\\n' | grep -vFx '${BIN_DIR}' | paste -sd ':' -)
REAL=$(PATH="$CLEAN_PATH" command -v '${cmd}' || true)
if [ -n "\${${sentinel}:-}" ] || [ -z "$REAL" ] || ! command -v sfw >/dev/null 2>&1; then
  [ -n "$REAL" ] && exec "$REAL" "$@"
  echo "${cmd}: not found" >&2; exit 127
fi
export ${sentinel}=1
exec sfw '${cmd}' "$@"
`
    const shim = path.join(BIN_DIR, cmd)
    writeFileSync(shim, body)
    chmodSync(shim, 0o755)
  }
  console.log(`[external-tools] wrote sfw shims for ${SFW_ECOSYSTEMS.join(', ')} in ${BIN_DIR}`)
  console.log(`[external-tools] prepend ${BIN_DIR} to PATH to activate`)
}

async function main(argv: string[] = process.argv.slice(2)): Promise<number> {
  if (argv.includes('--print-bin')) {
    console.log(BIN_DIR)
    return 0
  }
  const tools = loadTools()
  if (argv.includes('--check') || argv.length === 0) {
    const problems = checkPins(tools)
    for (const p of problems) {
      console.error(`[external-tools] ${p}`)
    }
    if (problems.length === 0) {
      console.log(`[external-tools] ${Object.keys(tools).length} pins valid`)
    }
    return problems.length === 0 ? 0 : 1
  }
  const installIdx = argv.indexOf('--install')
  if (installIdx !== -1) {
    await installTool(argv[installIdx + 1]!, tools)
  }
  if (argv.includes('--install-all')) {
    for (const name of Object.keys(tools)) {
      if (name === 'sfw-enterprise' || name === 'sfw-free') {
        continue
      }
      await installTool(name, tools)
    }
    await installTool('sfw', tools)
  }
  if (argv.includes('--shims')) {
    writeShims()
  }
  return 0
}

const isMain = process.argv[1] && import.meta.url === `file://${process.argv[1]}`
if (isMain) {
  main().then(
    code => {
      process.exitCode = code
    },
    err => {
      console.error(`[external-tools] ${err.message}`)
      process.exitCode = 1
    },
  )
}
