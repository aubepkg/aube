#!/usr/bin/env node
// Measure aube's opt-in transparent FS-compression of native addons
// (`AUBE_COMPRESS_STORE`, added in #985): the on-disk footprint AND the
// addon load time, compressed vs not.
//
// Size: for each package the script performs three fully isolated installs
// (own HOME, XDG dirs, cache, and store) — compression off, the default
// `**/*.node` gate, and the whole-store `glob:**/*` gate — then reports the
// store's logical vs physical bytes. Logical bytes must MATCH across modes
// (the compression is transparent: every reader sees identical bytes);
// physical bytes show the savings.
//
// Load time: every compressed addon is `require()`d in a fresh Node process,
// compressed vs uncompressed, in two scenarios: FIRST load of a fresh file
// (what an install pays once — dominated by the OS validating a
// freshly-created binary, for both variants) and STEADY-STATE loads of the
// same inode (what every later process pays). Fresh files are minted with
// copy-on-write clones (`cp -c` on macOS, COPYFILE_FICLONE_FORCE reflinks on
// Linux): a clone preserves the filesystem-compression attribute where a
// plain copy would silently write the file back out uncompressed and
// invalidate the comparison.
//
// Prerequisites:
//   - aube built in release mode: cargo build --release
//   - a filesystem with transparent compression (APFS on macOS, btrfs on
//     Linux, NTFS on Windows). On anything else the feature fails soft to
//     plain writes and off/on report the same physical size.
//
// Usage:
//   node benchmarks/fs-compress-size.mjs
//
// Environment variables:
//   PACKAGES — space-separated npm package names to measure
//              (default: "vite @rspack/core" — vite 8.x ships the rolldown +
//              lightningcss native addons; @rspack/core ships the ~40MB
//              @rspack/binding addon)
//   AUBE_BIN — aube binary to exercise (default: target/release/aube)
//   KEEP=1   — keep the scratch dir for inspection instead of deleting it

import { spawnSync } from 'node:child_process'
import {
  constants as fsConstants,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))
const packages = (process.env.PACKAGES ?? 'vite @rspack/core')
  .split(/\s+/)
  .filter(Boolean)
const aubeBin = path.resolve(
  process.env.AUBE_BIN ?? path.join(here, '..', 'target', 'release', 'aube'),
)

if (!existsSync(aubeBin)) {
  console.error(
    `error: aube binary not found at ${aubeBin} — run 'cargo build --release' first`,
  )
  process.exit(1)
}

const scratch = mkdtempSync(path.join(tmpdir(), 'aube-fs-compress-size-'))
process.on('exit', () => {
  if (process.env.KEEP === '1') {
    console.log(`scratch kept at ${scratch}`)
  } else {
    rmSync(scratch, { recursive: true, force: true })
  }
})

const kb = bytes => Math.floor(bytes / 1024)

// Walk a tree collecting every file's path plus logical (apparent) and
// physical (allocated) size. `blocks` is always 512-byte units in Node, so
// this is portable across APFS / btrfs / NTFS.
function walkSizes(dir, root = dir, files = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      walkSizes(full, root, files)
    } else if (entry.isFile()) {
      const st = statSync(full)
      files.push({
        full,
        rel: path.relative(root, full),
        logical: st.size,
        physical: st.blocks * 512,
      })
    }
  }
  return files
}

const median = values => [...values].sort((a, b) => a - b)[values.length >> 1]

// Clone (copy-on-write) a file, preserving the filesystem-compression
// attribute — a plain copy would silently write the data back out
// uncompressed. On macOS Node's COPYFILE_FICLONE_FORCE can fail with ENOSYS
// for decmpfs-compressed sources, so shell out to `cp -c` (raw clonefile);
// on Linux FICLONE_FORCE is the btrfs reflink path.
function cloneFile(src, dest) {
  if (process.platform === 'darwin') {
    const result = spawnSync('cp', ['-c', src, dest])
    if (result.status !== 0) {
      throw new Error(`clonefile failed: cp -c ${src} ${dest}`)
    }
    return
  }
  copyFileSync(src, dest, fsConstants.COPYFILE_FICLONE_FORCE)
}

// Time one `require()` of the addon in a fresh Node process. Returns
// milliseconds, or undefined for an addon that can't load standalone.
function loadMs(file) {
  const probe =
    'const s = process.hrtime.bigint();' +
    `require(${JSON.stringify(file)});` +
    'process.stdout.write(String(Number(process.hrtime.bigint() - s) / 1e6))'
  const result = spawnSync(process.execPath, ['-e', probe], {
    encoding: 'utf8',
  })
  return result.status === 0 ? Number(result.stdout) : undefined
}

// First-load vs steady-state timings for one on-disk variant of an addon.
// First load: clone → load → delete, x5 — clonefile mints a fresh vnode while
// preserving the compression attribute, so both variants pay the same
// fresh-file costs (on macOS, dominated by code-signature validation).
// Steady state: one clone loaded x7 in fresh processes, first sample dropped.
function timeVariant(src, dir, tag) {
  const first = []
  for (let i = 0; i < 5; i += 1) {
    const clone = path.join(dir, `${tag}-first-${i}.node`)
    cloneFile(src, clone)
    const ms = loadMs(clone)
    rmSync(clone)
    if (ms === undefined) {
      return undefined
    }
    first.push(ms)
  }
  const steadyClone = path.join(dir, `${tag}-steady.node`)
  cloneFile(src, steadyClone)
  const steady = []
  for (let i = 0; i < 7; i += 1) {
    steady.push(loadMs(steadyClone))
  }
  rmSync(steadyClone)
  return { first: median(first), steady: median(steady.slice(1)) }
}

// One isolated install. `gate` is the AUBE_COMPRESS_STORE value ('' = off).
// Returns the store's summed sizes.
function runInstall(name, pkg, gate) {
  const home = path.join(scratch, name)
  const project = path.join(home, 'project')
  mkdirSync(project, { recursive: true })
  writeFileSync(
    path.join(project, 'package.json'),
    JSON.stringify({
      name: 'probe',
      private: true,
      dependencies: { [pkg]: 'latest' },
    }),
  )
  // A minimal env so nothing leaks in from the host (a global aube config or
  // an inherited AUBE_COMPRESS_STORE would skew the comparison).
  // npm_config_minimum_release_age=0: measure the true latest publish.
  const result = spawnSync(aubeBin, ['install'], {
    cwd: project,
    env: {
      PATH: process.env.PATH,
      HOME: home,
      XDG_DATA_HOME: path.join(home, '.local', 'share'),
      XDG_CACHE_HOME: path.join(home, '.cache'),
      npm_config_minimum_release_age: '0',
      ...(gate ? { AUBE_COMPRESS_STORE: gate } : {}),
    },
    stdio: 'ignore',
  })
  if (result.status !== 0) {
    console.error(`error: '${aubeBin} install' failed for ${name}`)
    process.exit(1)
  }
  const store = path.join(home, '.local', 'share', 'aube', 'store')
  // Measure the CAS payload only (hash-addressed content, byte-stable across
  // runs) — the store also carries small index/metadata files whose bytes
  // legitimately vary run to run and would fail the transparency assert.
  const cas = path.join(store, 'v1', 'files')
  const files = walkSizes(existsSync(cas) ? cas : store)
  return {
    files,
    logicalKb: kb(files.reduce((s, f) => s + f.logical, 0)),
    physicalKb: kb(files.reduce((s, f) => s + f.physical, 0)),
  }
}

console.log(
  'aube store FS-compression size comparison (AUBE_COMPRESS_STORE)\n' +
    `binary: ${aubeBin}\n`,
)

// The three modes measured per package: gate value, scratch key, label.
const MODES = [
  ['', 'off', 'compression off       '],
  ['1', 'node-gate', 'default gate (*.node) '],
  ['glob:**/*', 'all-gate', "gate 'glob:**/*'      "],
]

const loadDir = path.join(scratch, 'load')
mkdirSync(loadDir, { recursive: true })

for (const pkg of packages) {
  const safe = pkg.replace(/[^a-zA-Z0-9]/g, '-')
  const pad = n => String(n).padStart(8)
  console.log(`── ${pkg} (latest) ──────────────────────────────────────────`)

  const runs = {}
  for (const [gate, key, label] of MODES) {
    const run = runInstall(`${safe}-${key}`, pkg, gate)
    runs[gate] = run
    const off = runs['']
    if (gate === '') {
      console.log(
        `  ${label}  ${pad(run.logicalKb)}KB logical  ${pad(run.physicalKb)}KB on disk`,
      )
      continue
    }
    if (run.logicalKb !== off.logicalKb) {
      console.error(
        '  ERROR: logical sizes differ across modes — compression must be\n' +
          '  byte-transparent; investigate before trusting these numbers.',
      )
      process.exit(1)
    }
    const savedPct = 100 - Math.floor((run.physicalKb * 100) / off.physicalKb)
    console.log(
      `  ${label}  ${pad(run.logicalKb)}KB logical  ${pad(run.physicalKb)}KB on disk  (−${savedPct}%)`,
    )
  }

  // Load-time comparison for every addon the default gate compressed. The
  // store is content-addressed, so the SAME rel path exists in the off store
  // with identical bytes — that copy is the uncompressed variant.
  console.log('  compressed addons — size and load time (compressed vs not):')
  const compressed = runs['1'].files.filter(
    f => f.logical > 1024 * 1024 && f.physical < f.logical * 0.95,
  )
  for (const f of compressed) {
    const plain = runs[''].files.find(p => p.rel === f.rel)
    const pct = 100 - Math.floor((f.physical * 100) / f.logical)
    const on = timeVariant(f.full, loadDir, 'on')
    const noff = plain && timeVariant(plain.full, loadDir, 'off')
    let timing = 'does not load standalone — timing skipped'
    if (on && noff) {
      timing =
        `first load ${on.first.toFixed(0)}ms vs ${noff.first.toFixed(0)}ms, ` +
        `steady ${on.steady.toFixed(1)}ms vs ${noff.steady.toFixed(1)}ms`
    }
    console.log(
      `    ${pad(kb(f.logical))}KB → ${pad(kb(f.physical))}KB (−${pct}%)  ${timing}`,
    )
  }
  console.log('')
}

console.log(
  'first load = fresh file (clonefile per iteration; both variants pay the\n' +
    "OS's freshly-created-binary validation, which dominates). steady = same\n" +
    'inode, fresh process per load, median of 6. Disk caches are warm in both\n' +
    'scenarios — flushing them needs privileges (`purge`) and is not attempted.',
)
