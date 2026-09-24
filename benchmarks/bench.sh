#!/usr/bin/env bash
set -euo pipefail

# Benchmark script comparing aube, pnpm, yarn (berry), npm, bun, deno,
# and vlt install performance.
#
# Prerequisites:
#   - aube built in release mode: cargo build --release, or AUBE_BIN set
#     to an executable released binary
#   - benchmark dependencies from mise (use `mise run bench` or
#     `mise run bench:bump`; missing package managers are skipped with
#     a warning rather than failing the whole run)
#
# Usage:
#   mise run bench
#
# Environment variables:
#   WARMUP       — warmup runs before timing (default: 1)
#   RUNS         — fixed timed runs for every tool. Unset (the default),
#                  tak picks each tool's count from how long its samples
#                  take: see `runs = "auto"` in benchmarks/tak.toml.
#   RESULTS_JSON — override the structured JSON output path
#   BENCH_TOOLS  — comma-separated tools to include
#                  (default: aube,bun,pnpm,npm,yarn,deno; vlt is
#                  temporarily disabled — its --frozen-lockfile still
#                  makes network requests, skewing results)
#   BENCH_SCENARIOS — comma-separated scenario keys to run
#                     (default: all)
#   BENCH_PHASES — set to 0 to skip aube phase timing samples
#   BENCH_SEED   — tak seed for the order samples are taken in. Each
#                  scenario prints the seed it used; pass it back to
#                  repeat that order.
#   AUBE_BIN     — override the aube executable to benchmark
#
#   BENCH_HERMETIC=1 — route all registry traffic through a local
#                      Verdaccio instance pre-populated from npmjs. This
#                      is the default for mise tasks; leave it on so
#                      cold-cache numbers are not npmjs/CDN latency tests.
#                      First hermetic run warms the cache at
#                      ~/.cache/aube-bench/registry/; subsequent runs
#                      are fully offline. See benchmarks/hermetic.bash.
#   BENCH_BANDWIDTH  — optional throttle (e.g. `50mbit`, `6mbit`, bare
#                      integer bytes/s). Defaults to `500mbit` in mise
#                      tasks; routes traffic through a tiny token-bucket
#                      proxy in front of Verdaccio.
#   BENCH_LATENCY    — optional fixed response latency for the throttle
#                      proxy. Defaults to `50ms` in mise tasks.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
AUBE_BIN="${AUBE_BIN:-$REPO_DIR/target/release/aube}"
PNPM_BIN="$(command -v pnpm || true)"
YARN_BIN="$(command -v yarn || true)"
NPM_BIN="$(command -v npm || true)"
BUN_BIN="$(command -v bun || true)"
DENO_BIN="$(command -v deno || true)"
VLT_BIN="$(command -v vlt || true)"

BENCH_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aube-bench.XXXXXX")"
WARMUP="${WARMUP:-1}"
# Unset means benchmarks/tak.toml's `runs = "auto"`: tak sizes each tool's
# run count from how long its samples take, within `min_runs`.
RUNS="${RUNS:-}"
BENCH_TOOLS="${BENCH_TOOLS:-aube,bun,pnpm,npm,yarn,deno}"
BENCH_SCENARIOS="${BENCH_SCENARIOS:-gvs-warm,gvs-cold,install-test}"
BENCH_PHASES="${BENCH_PHASES:-1}"

# ── Validation ──────────────────────────────────────────────────────────────

if ! command -v tak &>/dev/null; then
	echo "error: tak is required. Run via: mise run bench" >&2
	exit 1
fi
# benchmarks/tak.toml uses `runs = "auto"`, which arrived in tak 0.0.12
# alongside --no-progress; an older tak would reject the file.
if ! tak run --help 2>/dev/null | grep -q -- '--no-progress'; then
	echo "error: tak $(tak --version 2>/dev/null) is too old; 0.0.12 or newer is required" >&2
	exit 1
fi

if [ ! -f "$AUBE_BIN" ]; then
	echo "error: aube binary not found at $AUBE_BIN" >&2
	echo "Run: cargo build --release, or set AUBE_BIN to a released binary" >&2
	exit 1
fi

# ── Optional hermetic registry ─────────────────────────────────────────────
# BENCH_HERMETIC=1 routes all registry traffic through a local
# Verdaccio instance (populated from npmjs on first run, offline after).
# BENCH_BANDWIDTH=<rate> and BENCH_LATENCY=<delay> put a throttling
# proxy in front so cold-cache numbers reflect a simulated internet
# link rather than loopback disk speed. See benchmarks/hermetic.bash
# for the lifecycle details.

BENCH_REGISTRY_URL=""
if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/hermetic.bash"
	hermetic_start
	trap 'hermetic_stop' EXIT
fi

# ── Per-tool configuration ─────────────────────────────────────────────────
# Build up the list of tools to include dynamically so the matrix
# gracefully skips any pm that isn't installed. Each tool gets its
# own project dir, HOME, store, and cache so the scenarios are
# hermetic per-tool.

TOOLS=()
TOOL_BINS=()
TOOL_PROJECTS=()
TOOL_HOMES=()
TOOL_STORES=()
TOOL_CACHES=()

register_tool() {
	local name=$1 bin=$2
	case ",$BENCH_TOOLS," in
	*,"$name",*) ;;
	*) return ;;
	esac
	if [ -z "$bin" ] || [ ! -x "$bin" ]; then
		echo "warning: $name not found on \$PATH — skipping" >&2
		return
	fi
	TOOLS+=("$name")
	TOOL_BINS+=("$bin")
	TOOL_PROJECTS+=("$BENCH_DIR/project-$name")
	TOOL_HOMES+=("$BENCH_DIR/home-$name")
	TOOL_STORES+=("$BENCH_DIR/store-$name")
	TOOL_CACHES+=("$BENCH_DIR/cache-$name")
}

run_scenario() {
	local name=$1
	# `return 0`, not a bare `return`: that would pass on scenario_selected's
	# failure status, and `set -e` would end the whole run at the first
	# scenario left out of BENCH_SCENARIOS.
	scenario_selected "$name" || return 0

	shift
	"$@"
}

scenario_selected() {
	case ",$BENCH_SCENARIOS," in
	*,"$1",*) return 0 ;;
	*) return 1 ;;
	esac
}

# Order matters for the console output; keep aube first so the
# headline comparison is prominent and the rest follow alphabetically.
register_tool "aube" "$AUBE_BIN"
register_tool "bun" "$BUN_BIN"
register_tool "deno" "$DENO_BIN"
register_tool "pnpm" "$PNPM_BIN"
register_tool "npm" "$NPM_BIN"
register_tool "yarn" "$YARN_BIN"
register_tool "vlt" "$VLT_BIN"

# A durable progress line works in both an interactive terminal and Actions
# logs, unlike a carriage-return animation whose earlier states disappear.
# Count store population, every selected scenario/tool cell, and the optional
# aube-only phase samples as separate units of the complete benchmark run.
PROGRESS_COMPLETED=0
PROGRESS_STARTED=0
PROGRESS_TOTAL=${#TOOLS[@]}
# Each scenario is one tak run over every tool, so it is one unit.
for scenario in gvs-warm gvs-cold install-test; do
	if scenario_selected "$scenario"; then
		PROGRESS_TOTAL=$((PROGRESS_TOTAL + 1))
	fi
done
if [ "$BENCH_PHASES" != "0" ]; then
	for tool in "${TOOLS[@]}"; do
		[ "$tool" = "aube" ] || continue
		for scenario in gvs-warm gvs-cold; do
			if scenario_selected "$scenario"; then
				PROGRESS_TOTAL=$((PROGRESS_TOTAL + 1))
			fi
		done
	done
fi

progress_render() {
	local message=$1 width=24 filled empty fill blank percent
	filled=$((width * PROGRESS_COMPLETED / PROGRESS_TOTAL))
	empty=$((width - filled))
	percent=$((100 * PROGRESS_COMPLETED / PROGRESS_TOTAL))
	printf -v fill '%*s' "$filled" ''
	printf -v blank '%*s' "$empty" ''
	fill=${fill// /#}
	blank=${blank// /-}
	printf '[%s%s] %d/%d (%d%%) %s\n' \
		"$fill" "$blank" "$PROGRESS_COMPLETED" "$PROGRESS_TOTAL" "$percent" "$message"
}

progress_start() {
	PROGRESS_STARTED=$SECONDS
	progress_render "running $1"
}

progress_finish() {
	local label=$1 outcome=${2:-completed} elapsed
	elapsed=$((SECONDS - PROGRESS_STARTED))
	PROGRESS_COMPLETED=$((PROGRESS_COMPLETED + 1))
	progress_render "$outcome $label in ${elapsed}s"
}

if [ "$PROGRESS_TOTAL" -gt 0 ]; then
	progress_render "preparing benchmark matrix"
fi

echo "workdir: $BENCH_DIR"
# Capture each tool's reported --version string so generate-results.js
# can fold it into results.json. Some tools print extra text around
# the semver (e.g. `aube 1.0.0-beta.3 (...)`, `bun 1.3.12+...`); the
# sed pulls out the first token that looks like a semver so the JSON
# stays clean without the consumers having to re-parse it.
versions_file="$BENCH_DIR/versions.tsv"
: >"$versions_file"
for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	bin="${TOOL_BINS[$i]}"
	raw="$($bin --version 2>/dev/null || echo 'unknown')"
	version="$(printf '%s\n' "$raw" | head -n1 | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?' | head -n1)"
	[ -z "$version" ] && version="$raw"
	printf "%s\t%s\n" "$tool" "$version" >>"$versions_file"
	printf "%-5s %s  (%s)\n" "$tool:" "$bin" "$version"
done
node_version="$(node --version 2>/dev/null | sed 's/^v//')"
if [ -n "$node_version" ]; then
	printf "%s\t%s\n" "node" "$node_version" >>"$versions_file"
	printf "%-5s %s\n" "node:" "$node_version"
fi
export BENCH_VERSIONS_FILE="$versions_file"
echo ""

# Per-tool lockfile filename (the name the pm writes into the project
# directory after `install`). Used to decide what to save after the
# populate step and where to copy it back for the "warm lockfile"
# scenarios.
lockfile_name_for() {
	case "$1" in
	aube) echo "aube-lock.yaml" ;;
	bun) echo "bun.lock" ;;
	deno) echo "deno.lock" ;;
	npm) echo "package-lock.json" ;;
	pnpm) echo "pnpm-lock.yaml" ;;
	yarn) echo "yarn.lock" ;;
	vlt) echo "vlt-lock.json" ;;
	*) echo "unknown" ;;
	esac
}

# ── Project setup ──────────────────────────────────────────────────────────

for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	dir="${TOOL_PROJECTS[$i]}"
	home="${TOOL_HOMES[$i]}"
	mkdir -p "$dir" "$home" "${TOOL_CACHES[$i]}"
	cp "$SCRIPT_DIR/fixture.package.json" "$dir/package.json"

	# pnpm reads storeDir / cacheDir from pnpm-workspace.yaml; the
	# other tools take them via CLI flags or env vars at command
	# time, so nothing to write on disk up front.
	if [ "$tool" = "pnpm" ]; then
		printf "storeDir: %s\ncacheDir: %s\n" "${TOOL_STORES[$i]}" "${TOOL_CACHES[$i]}" >"$dir/pnpm-workspace.yaml"
	fi

	# Yarn 4 ignores .npmrc for registry and only ships a PnP linker by
	# default. Drop a .yarnrc.yml that pins node-modules layout (so the
	# scenarios mirror what npm/pnpm/bun produce), routes the cache to
	# the isolated dir, disables telemetry, and turns off lifecycle
	# scripts so it matches the --ignore-scripts behavior we ask from
	# the other tools. The hermetic registry URL gets injected lower
	# down once we know BENCH_REGISTRY_URL.
	if [ "$tool" = "yarn" ]; then
		{
			printf "nodeLinker: node-modules\n"
			printf "cacheFolder: %s\n" "${TOOL_CACHES[$i]}"
			printf "enableGlobalCache: false\n"
			printf "enableTelemetry: false\n"
			printf "enableScripts: false\n"
			# Yarn 4 auto-enables immutable installs when it detects
			# `CI=true`, which makes the warm step refuse to create
			# the initial lockfile (YN0028) and crashes bench-refresh.
			# Pin the flag off so the warm install can populate the
			# lockfile + cache + store the same way it does locally.
			printf "enableImmutableInstalls: false\n"
		} >"$dir/.yarnrc.yml"
	fi

	# Hermetic mode: drop a .npmrc into both the project dir and the
	# isolated HOME so every PM resolves packages through the local
	# Verdaccio (or the throttle proxy in front of it) instead of
	# npmjs. Project-level .npmrc is honored by aube/pnpm/npm/bun/
	# deno/vlt and wins over HOME; HOME is a belt-and-suspenders
	# fallback for any command (like `aube add` after chdir) that
	# might look there first. Yarn 4 ignores .npmrc and reads the
	# registry from .yarnrc.yml instead, so we append it there.
	if [ -n "$BENCH_REGISTRY_URL" ]; then
		printf "registry=%s\n" "$BENCH_REGISTRY_URL" >"$dir/.npmrc"
		printf "registry=%s\n" "$BENCH_REGISTRY_URL" >"$home/.npmrc"
		if [ "$tool" = "yarn" ]; then
			printf "npmRegistryServer: \"%s\"\nunsafeHttpWhitelist:\n  - 127.0.0.1\n  - localhost\n" \
				"$BENCH_REGISTRY_URL" >>"$dir/.yarnrc.yml"
		fi
	fi
done

# Keep a pristine copy of package.json
cp "$SCRIPT_DIR/fixture.package.json" "$BENCH_DIR/original-package.json"

# ── Populate stores and caches ─────────────────────────────────────────────
# One warm install per tool so the lockfile + cache + store are all
# populated before the scenario matrix runs. Everything is hermetic
# (isolated HOME / cache / store), so this is safe to run in parallel
# in the future but we keep it serial for clear console output.
#
# Bracket the populate loop with uplink-enabled Verdaccio so any
# package the warm step missed (e.g. when a PM's resolution diverges
# between the warm fixture and the bench fixture) gets fetched from
# npmjs and cached locally. The cold config is restored afterwards so
# the timed scenarios remain hermetic.
if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	hermetic_use_warm_uplink
fi

for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	dir="${TOOL_PROJECTS[$i]}"
	bin="${TOOL_BINS[$i]}"
	home="${TOOL_HOMES[$i]}"
	cache="${TOOL_CACHES[$i]}"
	lockfile_name=$(lockfile_name_for "$tool")
	progress_start "populate/$tool"
	echo "Populating store and cache for $tool..."
	# Wipe every known lockfile so an earlier failed run doesn't
	# leave a stale one behind that would fool the pm into a
	# different code path.
	rm -rf "$dir/node_modules" \
		"$dir/pnpm-lock.yaml" \
		"$dir/aube-lock.yaml" \
		"$dir/package-lock.json" \
		"$dir/yarn.lock" \
		"$dir/bun.lock" \
		"$dir/bun.lockb" \
		"$dir/deno.lock" \
		"$dir/vlt-lock.json"

	case "$tool" in
	aube)
		# Aube's built-in trusted-dependency list can allow known-safe
		# install scripts; opt out explicitly to match every other PM.
		cd "$dir" && HOME="$home" XDG_CACHE_HOME="$cache" XDG_DATA_HOME="$home/.local/share" "$bin" install --ignore-scripts
		;;
	npm)
		# `--legacy-peer-deps` is the only way npm tolerates the
		# fixture's mixed peer-dep ranges (eslint 9 vs 8, etc.).
		# pnpm/aube handle this via `autoInstallPeers=true` by
		# default; using npm's strict mode here would just make
		# the populate step fail before we even reach the
		# scenarios. Yes, this is the classic "npm is stricter"
		# caveat you read in every benchmark footnote.
		cd "$dir" && HOME="$home" npm_config_cache="$cache" "$bin" install \
			--ignore-scripts --no-audit --no-fund --legacy-peer-deps
		;;
	pnpm)
		cd "$dir" && HOME="$home" "$bin" install --ignore-scripts --no-frozen-lockfile
		;;
	yarn)
		# Yarn 4 (berry). enableScripts/cacheFolder/nodeLinker are
		# already pinned in .yarnrc.yml, so we only need to ask for
		# a fresh install here.
		cd "$dir" && HOME="$home" "$bin" install
		;;
	bun)
		# Bun takes `--cache-dir` as a CLI flag and `BUN_INSTALL` as
		# the global install prefix. Point both at the hermetic temp
		# to keep it from touching `~/.bun`.
		cd "$dir" && HOME="$home" BUN_INSTALL="$home/.bun" "$bin" install \
			--cache-dir "$cache" --ignore-scripts --no-summary --force
		;;
	deno)
		# Deno 2 reads package.json and writes deno.lock + populates
		# node_modules. DENO_DIR is the per-tool cache and global
		# install location. Lifecycle scripts are skipped by default
		# (Deno requires explicit --allow-scripts to opt in).
		cd "$dir" && HOME="$home" DENO_DIR="$cache" "$bin" install --quiet
		;;
	vlt)
		# vlt respects npm_config_cache for its package cache and
		# reads .npmrc for the registry. Skips lifecycle scripts by
		# default unless an allowlist is configured.
		cd "$dir" && HOME="$home" npm_config_cache="$cache" "$bin" install
		;;
	esac

	if [ ! -f "$dir/$lockfile_name" ]; then
		echo "error: $lockfile_name was not created for $tool in $dir" >&2
		exit 1
	fi
	cp "$dir/$lockfile_name" "$BENCH_DIR/saved-lockfile-$tool"
	progress_finish "populate/$tool"
done

if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	hermetic_use_no_uplink
fi

# ── Scenarios ──────────────────────────────────────────────────────────────
#
# The scenario commands live in benchmarks/tak.toml, one benchmark per
# scenario and one subject per tool. They run under `sh -c` and read the
# per-tool paths and settings exported below.

# Minimum publish-age gate, in minutes. aube defaults to 1440 (24h)
# as a supply-chain mitigation — the resolver skips versions newer
# than this window. The default forces aube to fetch the full
# (non-corgi) packument format so it can read the per-version `time`
# map; corgi omits `time` on npmjs.org. Most modern PMs support an
# equivalent flag, each with their own unit:
#
#   aube  minimumReleaseAge          (minutes; default 1440)
#   npm   --min-release-age          (days)
#   pnpm  --config.minimum-release-age (minutes)
#   bun   --minimum-release-age      (seconds)
#   deno  --minimum-dependency-age   (minutes, marked Unstable)
#   yarn  not supported
#   vlt   not investigated (currently disabled in BENCH_TOOLS anyway)
#
# Pinning all supported PMs to the same value makes the bench an
# apples-to-apples comparison — otherwise aube alone pays the
# full-packument cost (5x larger response on @types/node and similar
# heavily-versioned packuments) while bun/pnpm cruise on corgi.
#
# Override via `BENCH_MIN_RELEASE_AGE_MINUTES=0` to disable the gate
# across all PMs (useful for measuring raw resolver speed without
# the security-feature axis).
MIN_RELEASE_AGE_MINUTES="${BENCH_MIN_RELEASE_AGE_MINUTES:-1440}"
MIN_RELEASE_AGE_SECONDS=$((MIN_RELEASE_AGE_MINUTES * 60))
# npm uses days as the unit. Round up so the gate is at least as
# strict as aube's, never weaker. (60*24 = 1440 → 1 day exactly.)
MIN_RELEASE_AGE_DAYS=$(((MIN_RELEASE_AGE_MINUTES + 60 * 24 - 1) / (60 * 24)))

export BENCH_DIR MIN_RELEASE_AGE_MINUTES MIN_RELEASE_AGE_SECONDS MIN_RELEASE_AGE_DAYS
export AUBE_BIN BUN_BIN DENO_BIN PNPM_BIN NPM_BIN YARN_BIN VLT_BIN

# The tools this run measures, as tak `--subject` flags: those selected by
# BENCH_TOOLS that are also installed.
SUBJECT_ARGS=()
for tool in "${TOOLS[@]}"; do
	SUBJECT_ARGS+=(--subject "$tool")
done

TAK_ARGS=(--no-counters --warmup "$WARMUP")
if [ -n "$RUNS" ]; then
	TAK_ARGS+=(--runs "$RUNS")
fi
if [ -n "${BENCH_SEED:-}" ]; then
	TAK_ARGS+=(--seed "$BENCH_SEED")
fi

# Measure one scenario across every tool: `tak run --bench <scenario>` from
# this directory, so tak finds benchmarks/tak.toml rather than the
# repository's instruction-count one.
run_bench() {
	local bench_name=$1
	# With no --subject at all tak would run every subject; there is nothing
	# to measure instead.
	[ "${#TOOLS[@]}" -gt 0 ] || return 0
	progress_start "$bench_name"
	echo ""
	# A tool that fails is dropped from the rest of the run and left out of
	# the export; generate-results.js reports it as n/a. tak exits non-zero
	# for that, which must not abort the other scenarios.
	if ! (cd "$SCRIPT_DIR" && tak run --bench "$bench_name" "${TAK_ARGS[@]}" \
		"${SUBJECT_ARGS[@]}" --export-json "$BENCH_DIR/${bench_name}.json"); then
		echo "warning: one or more tools failed in $bench_name; they are missing from the results" >&2
	fi
	progress_finish "$bench_name"
}

PHASES_FILE="$BENCH_DIR/aube-install-phases.jsonl"
: >"$PHASES_FILE"

# One aube install per scenario with phase timing on, for attribution: the
# binary writes resolve/fetch/link/script/state timings as JSONL. It runs the
# scenario's own aube subject from benchmarks/tak.toml, once, with the phase
# variables in its environment, so the command cannot drift from the timed one.
run_aube_phase_bench() {
	local bench_name=$1
	case " ${TOOLS[*]} " in
	*" aube "*) ;;
	*) return 0 ;;
	esac
	progress_start "phases/$bench_name"
	echo "  $bench_name"
	if ! (cd "$SCRIPT_DIR" && AUBE_BENCH_PHASES_FILE="$PHASES_FILE" AUBE_BENCH_SCENARIO="$bench_name" \
		tak run --bench "$bench_name" --subject aube --runs 1 --warmup 0 --no-counters --no-progress \
		>/dev/null); then
		echo "warning: phase timing failed for $bench_name - skipping sample" >&2
		progress_finish "phases/$bench_name" "skipped"
		return 0
	fi
	progress_finish "phases/$bench_name"
}

# ── Benchmark 1: Fresh install, warm cache ─────────────────────────────────
# Lockfile present, node_modules deleted, store and cache warm.
# Pins aube's default local global virtual store behavior so GitHub
# Actions' inherited CI=true environment cannot silently turn this into
# per-project mode.

echo ""
echo "━━━ Benchmark 1: Fresh install (warm cache) ━━━"
run_scenario "gvs-warm" run_bench "gvs-warm"

# ── Benchmark 2: Fresh install, cold cache ─────────────────────────────────
# Lockfile present, but store and cache are empty.
# Measures fetch-from-registry + import + link/materialization work.

echo ""
echo "━━━ Benchmark 2: Fresh install (cold cache) ━━━"
run_scenario "gvs-cold" run_bench "gvs-cold"

# ── Aube phase timing sample ───────────────────────────────────────────────
# tak discards stdout/stderr and times whole commands. For attribution,
# run aube once per install-shaped scenario with AUBE_BENCH_PHASES_FILE
# enabled so the binary writes structured resolve/fetch/link/script/state
# timings to JSONL, then summarize it at the end.

echo ""
echo "━━━ Aube install phase timings ━━━"
if [ "$BENCH_PHASES" != "0" ]; then
	run_scenario "gvs-warm" run_aube_phase_bench "gvs-warm"
	run_scenario "gvs-cold" run_aube_phase_bench "gvs-cold"
fi

# ── Benchmark 3: install + run test (developer loop) ───────────────────────
# Warm store+cache, lockfile present, node_modules *already* populated.
# Models the developer-loop case: "I've installed, now I keep re-running
# my tests." Each iteration's prepare runs the full install-test command
# once (untimed) so node_modules and any tool-specific state files are
# valid, then the timed iteration re-runs the same command. Tools with a
# state-based short-circuit (aube's .aube-state) skip install entirely
# on the timed run; tools without one still pay for lockfile revalidation.

echo ""
echo "━━━ Benchmark 3: install + run test (already installed) ━━━"
run_scenario "install-test" run_bench "install-test"

# ── Summary ────────────────────────────────────────────────────────────────

RESULTS_MD="$BENCH_DIR/results.md"

echo ""
echo "━━━ Results ━━━"
TOOLS_CSV=$(
	IFS=,
	echo "${TOOLS[*]}"
)
BENCH_TOOLS="$TOOLS_CSV" BENCH_SCENARIOS="$BENCH_SCENARIOS" node "$SCRIPT_DIR/generate-results.js" "$BENCH_DIR" "$RESULTS_MD"
if [ -s "$PHASES_FILE" ]; then
	echo ""
	node "$SCRIPT_DIR/generate-phase-results.mts" "$PHASES_FILE" "$BENCH_DIR/aube-install-phases.md"
fi
echo ""
echo "Results saved to: $RESULTS_MD"
echo ""
echo "Temp directory kept at: $BENCH_DIR"
echo "Remove with: rm -rf $BENCH_DIR"
