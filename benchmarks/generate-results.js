// Reads the per-scenario tak JSON output from `bench.sh` and
// emits two artifacts:
//
//   1. A human-readable markdown summary at `outputFile` (same format
//      the console prints).
//   2. A structured JSON file at `<outputFile without extension>.json`
//      so downstream consumers — notably `docs/benchmarks.data.ts`,
//      the VitePress data loader behind the `<BenchChart>` on the
//      benchmarks page — can ingest the results without parsing
//      markdown.
//
// Usage:
//   node generate-results.js <benchDir> <outputMarkdown>
// Optional env:
//   BENCH_TOOLS=aube,bun,pnpm,npm,yarn,deno,vlt
//                                   comma-separated tool order
//                                   (defaults to aube + pnpm)
//   RESULTS_JSON=<path>             override the JSON output path

const fs = require('fs')
const path = require('path')

const benchDir = process.argv[2]
const outputFile = process.argv[3]

const benchmarks = [
  ['gvs-warm', 'Fresh install (warm cache)'],
  ['gvs-cold', 'Fresh install (cold cache)'],
  ['install-test', 'npm install && npm run test'],
]
const SELECTED_BENCHMARKS = new Set(
  (process.env.BENCH_SCENARIOS || benchmarks.map(([name]) => name).join(','))
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean),
)

const TOOLS = (process.env.BENCH_TOOLS || 'aube,pnpm')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean)

// One `tak run --export-json` file per scenario, with an entry per tool
// (hyperfine's result shape plus `subject`). A tool that failed is absent
// from it and reported as n/a.
//
// The published value is the median. A few slow samples from a busy
// runner, or a first sample that pays a one-time cost, would otherwise
// drag the mean far from a typical install and decide a close comparison.
function readResult (benchDir, name, tool) {
  try {
    const data = JSON.parse(fs.readFileSync(`${benchDir}/${name}.json`, 'utf8'))
    const r = data.results.find((result) => result.subject === tool)
    if (!r) return missing()
    if (!Number.isFinite(r.median) || !Number.isFinite(r.mean)) {
      throw new Error('missing benchmark median or mean')
    }
    const stddev = Number.isFinite(r.stddev) ? r.stddev : 0
    return {
      text: `${r.median.toFixed(3)}s (mean ${r.mean.toFixed(3)}s ± ${stddev.toFixed(3)}s)`,
      median: r.median,
      mean: r.mean,
      stddev,
      min: r.min,
      max: r.max,
    }
  } catch (err) {
    if (err && err.code !== 'ENOENT') {
      console.error(`Warning: failed to read ${name}/${tool}: ${err.message}`)
    }
    return missing()
  }
}

function missing () {
  return { text: 'n/a', median: null, mean: null, stddev: null, min: null, max: null }
}

function fmtSpeedup (base, aube) {
  if (base == null || aube == null) return ''
  if (aube < base) {
    return ` (${(base / aube).toFixed(1)}x faster)`
  } else if (aube > base) {
    return ` (${(aube / base).toFixed(1)}x slower)`
  }
  return ''
}

// -- Markdown ---------------------------------------------------------------
// Emits one row per scenario with a column per tool plus trailing
// "vs pnpm" and "vs bun" speedup columns when those tools are present
// in the run. pnpm is aube's drop-in-replacement target; bun is the
// other "fast" package manager users compare against.
const headerCells = ['#', 'Scenario', ...TOOLS]
if (TOOLS.includes('pnpm') && TOOLS.includes('aube')) {
  headerCells.push('vs pnpm')
}
if (TOOLS.includes('bun') && TOOLS.includes('aube')) {
  headerCells.push('vs bun')
}

const lines = [
  '# Benchmark Results',
  '',
  `| ${headerCells.join(' | ')} |`,
  `|${headerCells.map(() => '---').join('|')}|`,
]

// -- Structured JSON --------------------------------------------------------
// tak records each tool's `--version` output (`version_cmd` in
// benchmarks/tak.toml) and the machine it ran on in every scenario's export.
// Versions are trimmed to the first semver-looking token so the docs chart
// shows `1.4.2` rather than `bun 1.4.2+abc (…)`.
const versions = {}
let machine = null
for (const [name] of benchmarks) {
  let data
  try {
    data = JSON.parse(fs.readFileSync(`${benchDir}/${name}.json`, 'utf8'))
  } catch {
    continue
  }
  machine ??= data.machine ?? null
  for (const r of data.results ?? []) {
    if (versions[r.subject] || typeof r.version !== 'string') continue
    const semver = r.version.match(/[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?/)
    versions[r.subject] = semver ? semver[0] : r.version.trim()
  }
}
versions.node = process.versions.node

const json = {
  updated: new Date().toISOString(),
  unit: 'ms',
  managers: TOOLS,
  versions,
  machine,
  rows: [],
}

benchmarks.filter(([name]) => SELECTED_BENCHMARKS.has(name)).forEach(([name, label], i) => {
  const results = {}
  for (const tool of TOOLS) {
    results[tool] = readResult(benchDir, name, tool)
  }

  const cells = [String(i + 1), label]
  for (const tool of TOOLS) {
    cells.push(results[tool].text)
  }
  if (TOOLS.includes('pnpm') && TOOLS.includes('aube')) {
    cells.push(fmtSpeedup(results.pnpm.median, results.aube.median).trim())
  }
  if (TOOLS.includes('bun') && TOOLS.includes('aube')) {
    cells.push(fmtSpeedup(results.bun.median, results.aube.median).trim())
  }
  lines.push(`| ${cells.join(' | ')} |`)

  const values = {}
  const stats = {}
  for (const tool of TOOLS) {
    values[tool] = results[tool].median == null ? null : Math.round(results[tool].median * 1000)
    stats[tool] = results[tool].median == null ? null : results[tool]
  }

  json.rows.push({ key: name, label, values, stats })
})

lines.push('')

const output = lines.join('\n')
fs.writeFileSync(outputFile, output)
console.log(output)

const jsonOut = process.env.RESULTS_JSON
  || `${outputFile.replace(/\.md$/, '')}.json`
fs.writeFileSync(jsonOut, JSON.stringify(json, null, 2) + '\n')
console.log(`Wrote structured results to ${path.resolve(jsonOut)}`)
