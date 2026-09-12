# RFC: Declarative Binary Artifacts for npm Packages

| | |
|---|---|
| **Status** | Draft |
| **Author** | Jeff Dickey ([@jdx](https://github.com/jdx)) |
| **Created** | 2026-08-13 |
| **Intended venue** | OpenJS Package Metadata Interoperability Working Group, with npm/rfcs as the follow-on implementation venue for the npm CLI |

## Summary

Native tools and libraries should install as **declarative package artifacts selected by the package manager** — not execute code at install time to discover and fetch themselves, and not launch through a JavaScript trampoline on every invocation.

This RFC proposes a new top-level manifest field, **`artifacts`**, which formalizes the platform-package pattern that esbuild, sharp, and the napi-rs ecosystem already hand-roll. A package declares named **slots**; each slot lists ordered **candidate packages** — ordinary registry packages — guarded by declarative platform predicates (`os`, `cpu`, `libc`, `napi`, `engines.node`); a predicate-free final candidate serves as the fallback tier (WASM or pure JS). A conforming package manager evaluates the predicates at install time, locks *all* candidates so one lockfile serves every platform, and materializes exactly one:

- under its real name (unchanged — it is an ordinary optional dependency),
- at a **stable alias** (`_<slot>`) the parent package's code resolves at runtime — no try/catch over N package names, and
- as **direct bin links** to the executable the artifact itself declares (via the artifact-side `artifact` field), overriding the parent's legacy JS-shim bins — no Node.js trampoline at launch.

Package managers that predate the standard ignore the field and get today's working behavior; the legacy surface remains the authoritative fallback. Where that fallback is an install script, the package declares it superseded and a conforming package manager never runs it. No lifecycle scripts, no registry protocol changes, no Node.js resolver changes, no flag day.

A whole-package substitution model in the lineage of npm RFC [#519](https://github.com/npm/rfcs/pull/519) and Yarn's [Package Variants](https://github.com/yarnpkg/berry/issues/2751) was seriously considered and is presented as an alternative in *Rationale and Alternatives*, with a full comparison.

## Motivation

Prebuilt native code is now the norm, not the exception: bundlers (esbuild, rollup's and SWC's native cores), linters and formatters (Biome, oxc), database engines (Prisma), cryptography and image libraries (bcrypt, sharp), and whole CLIs distributed via npm (turbo, aube, mise, sentry-cli). The npm ecosystem has no standard way to ship them. Every project chooses one of two workarounds, and both have serious costs.

### Workaround 1: install scripts

The package ships a `postinstall`/`preinstall` script that detects the platform and downloads a binary (node-pre-gyp, prisma's engine fetcher, sentry-cli, and — self-deprecatingly — aube's own npm package, whose `preinstall` runs [`installArchSpecificPackage.js`](https://github.com/jdx/aube/blob/main/npm/installArchSpecificPackage.js)).

Problems:

- **Arbitrary code execution at install time.** Install scripts are the primary supply-chain attack surface in the npm ecosystem. Security-conscious installers disable them (`--ignore-scripts`), and package managers increasingly default to not running them, at which point the package silently doesn't work. Aube's own installation docs must instruct users to pass `--ignore-scripts=false`, defeating a hardening default because of the very pattern this RFC replaces.
- **Bypasses the registry contract.** Downloads sidestep the package manager's cache, lockfile, SHA-512 integrity verification, provenance attestations, registry mirrors, and offline installs. A lockfile no longer describes what ends up on disk.
- **Unreproducible.** The fetched artifact is whatever the remote endpoint serves that day; corporate proxies, air-gapped environments, and immutable CI caches all break.

### Workaround 2: `optionalDependencies` + a hand-rolled loader

The package publishes one platform package per target (`@esbuild/linux-x64`, `@img/sharp-darwin-arm64`, `@node-rs/bcrypt-linux-x64-gnu`, …), lists all of them as `optionalDependencies` with `os`/`cpu`/`libc` fields so package managers filter to the installable one, and ships JavaScript that locates the installed package at runtime via a try/catch chain over the possible names.

This is strictly better than install scripts — artifacts are real registry packages with integrity and provenance — and it is the pattern this RFC builds on rather than discards. But everything above the `optionalDependencies` line is reinvented per project:

- **Hand-rolled platform detection.** Each project re-implements os/cpu/libc probing in JavaScript (glibc-vs-musl detection alone has enough edge cases that [`detect-libc`](https://www.npmjs.com/package/detect-libc) exists, and still misdetects in containers and static-binary contexts).
- **Runtime try/catch resolution.** The loader attempts `require()` over N package names and picks the first that doesn't throw. It runs on *every process start*, and its failure mode is a confusing `MODULE_NOT_FOUND` cascade rather than an actionable "no artifact for linux/riscv64".
- **Lockfile and install noise.** Every platform's artifact appears in every lockfile; optional-dependency install failures (`EBADPLATFORM`) are routinely warned about and routinely ignored, training users to ignore warnings.
- **The package manager already knows everything the loader is trying to discover** — it selected which optional dependency to install. The runtime detection is a reconstruction of a decision the installer already made and threw away.

### The CLI shim tax

For CLI tools the second workaround has an additional, permanent cost that deserves its own callout, because eliminating it is a primary motivation of this proposal.

A native CLI shipped via npm today launches like this:

```
user runs `esbuild` → .bin shim → Node.js starts (~50–100ms) →
JS wrapper resolves the platform package → spawns the real binary →
two processes for the lifetime of the invocation
```

The `bin` field can only point at the JavaScript wrapper, because at publish time the package doesn't know which platform package will be installed. So every invocation, on every machine, forever, pays full Node.js interpreter startup plus an extra process and the signal-forwarding/exit-code plumbing between them — to solve a problem that exists only at install time. For fast native tools the shim is frequently *more expensive than the tool's own work*.

A package manager that performs artifact selection at install time can link the `bin` entry **directly to the native executable**. This RFC makes that a conformance requirement, not an optimization.

### Both use cases matter

Prior discussions of this problem have often centered on CLI tools, but the larger population is **libraries**: N-API addons loaded with `require()` (bcrypt, sharp, @swc/core, everything built with napi-rs or node-gyp) and binary payloads that JavaScript spawns or reads (Prisma's query engines). A standard that only solves `bin` linking leaves most native packages on the old patterns. This RFC treats three consumption surfaces as first-class: **executables**, **addons/modules loaded at runtime**, and **plain file payloads**, with WASM or pure-JS implementations as explicit fallback tiers of the same mechanism.

## Detailed Explanation

### Invariants

These are the contract of the standard; the `artifacts` field is a mechanism that satisfies them, and any future extension must too.

1. **Artifacts are ordinary registry packages.** They resolve through normal registries, appear in lockfiles with exact versions and SHA-512 integrity, are served from caches and mirrors, work offline, and can carry npm provenance attestations. No registry protocol changes.
2. **No lifecycle scripts.** The mechanism requires no `preinstall`/`postinstall` anywhere. Artifact packages must be passive carriers: conforming package managers **must not** execute lifecycle scripts of packages installed via artifact selection. A parent's own install script kept as the legacy fallback is declared superseded and is never executed when its slot selects an artifact (see *Lifecycle-script supersession*).
3. **Selection is declarative and performed by the package manager.** The matching language is non-Turing-complete and statically analyzable. Packages do not ship platform-detection code on the conforming path.
4. **Progressive enhancement, no flag day.** The new manifest field is ignored by package managers that predate it. A published package also ships today's compat surface (JS shim `bin`, loader chain, `optionalDependencies`), which continues to work unchanged on legacy installers. The legacy surface is the **authoritative fallback**: a conforming package manager that selects no artifact must behave exactly like a non-conforming one. Partial implementations are therefore safe by construction.
5. **No runtime shim on the CLI path.** When a selected artifact declares its executables (the `artifact` field), conforming package managers **must** expose those `bin` commands as direct links to the native executable — a symlink, or an executable copy, on Unix; a real `.exe` on Windows (never interpreted through a shebang/`cmd` shim that assumes a Node script). A hardlink is valid only when its source inode already has the required executable mode; implementations must never chmod through a hardlink into an immutable content-addressed store. Zero interpreter trampoline, zero extra process at launch. A JS shim may exist in the package only as the legacy fallback surface; a conforming implementation never executes it.
6. **One lockfile for every platform.** *All* declared candidates are resolved and locked (name, exact version, integrity — a metadata-only operation; no tarball downloads for non-selected candidates). Only the selected artifact is materialized on disk. Selection is a pure function of `(manifest, target tuple)` and is **never recorded in the lockfile**: recording it would reintroduce the platform-dirty lockfiles this design exists to eliminate.
7. **Implementable by npm, pnpm, Yarn, Bun, and aube** without coordinated releases, and without changes to Node.js module resolution.

### The selection primitive

#### Target tuple

Selection is evaluated against a target tuple:

```
(os, cpu, libc, nodeVersion)
```

- `os`, `cpu`: values of `process.platform` / `process.arch` (`linux`, `darwin`, `win32`, …; `x64`, `arm64`, …).
- `libc`: `"glibc"` or `"musl"` on Linux; absent elsewhere. Detection **must** be a runtime probe (the `detect-libc` / `process.report().header.glibcVersionRuntime` family of heuristics), never a compile-time constant of the package manager itself — a statically-linked musl build of a package manager running on a glibc host must still report `glibc`. The normative probe order is the dynamic loader in `/proc/self/maps` first, then `ld-linux*`/`ld-musl-*` filesystem probes, with glibc winning conflicting evidence. This requirement governs package-manager artifact selection; legacy package loaders are fallback implementations outside this RFC and may use their existing runtime detection.
- `nodeVersion`: the version of Node.js the project will run — by default the package manager's runtime; overridable by fields like `devEngines.runtime` where supported.

Package managers **must** support explicit target-tuple overrides beyond the host (configuration and/or CLI flag), for Docker cross-platform installs and pnpm-`supportedArchitectures`-style fleet caching. Multi-tuple behavior is specified under *Lockfiles* below.

#### Candidate predicates

Each candidate artifact carries zero or more predicates:

| Predicate | Type | Matches when | Support |
|---|---|---|---|
| `os` | string \| string[] | target `os` ∈ listed values | required |
| `cpu` | string \| string[] | target `cpu` ∈ listed values | required |
| `libc` | `"glibc"` \| `"musl"` | target os is `linux` **and** detected libc equals the value | required |
| `napi` | integer | N-API version of target `nodeVersion` ≥ value | optional |
| `engines.node` | semver range | target `nodeVersion` satisfies the range | optional |

Rules:

- Predicates within a candidate are **conjunctive**; array values within one predicate are **disjunctive**. An omitted predicate matches anything. Value semantics for `os`/`cpu`/`libc` are identical to the existing manifest fields of the same names (including npm ≥ 10.4's `libc` handling), so one set of semantics covers both.
- **`napi` is evaluated against the target tuple's `nodeVersion`, never against the package manager's own runtime.** N-API versions map to Node.js releases, so the value is derivable without executing the target Node. Comparing against the installing runtime is the install-machine-vs-run-machine trap in miniature: an artifact selected under install-time Node 22 must not fail on the project's Node 18 runtime.
- **Unknown predicate keys cause the candidate to be treated as non-matching.** This is the forward-compatibility rule: when a future revision adds (say) `cpuFeatures`, an older conforming package manager skips those candidates and falls through to broader ones or to the fallback tier — degrading to a safe choice instead of selecting an artifact whose constraint it cannot check.
- **First match wins, in author order.** There is no specificity scoring. Scoring would require five independent implementations to reproduce a ranking function bit-for-bit forever; author order moves the judgment to the publisher, who knows that the AVX-tuned build should be listed before the baseline build and native before WASM.
- **CPU micro-architecture features (AVX2, NEON, …) are deliberately excluded from v1.** The install machine is not the run machine (an image built on an AVX-512 CI host may deploy anywhere); install-time selection on CPU features produces `SIGILL` in production. Feature dispatch belongs inside the artifact at runtime. The key `cpuFeatures` is reserved, and the unknown-key rule above means a future revision introducing it degrades safely on v1 implementations.
- **The v1 predicate set is the intersection all five package managers can check today.** The known demands beyond it — OpenSSL/TLS-library variants (Prisma's `rhel-openssl-1.0.x` targets), ARM sub-architecture (`armv6`/`armv7`), minimum OS or kernel version, minimum glibc version, alternate runtimes (Electron, NW.js) — are all expressible as future predicate keys, and the unknown-key rule means each degrades safely on older implementations the day it ships. They are deferred rather than rejected: every added key multiplies the conformance matrix five implementations must agree on, and several are open design questions in their own right (see *Unresolved Questions*).
- A predicate-free candidate is a catch-all: it always matches, so anything after it is unreachable. Listed last, it **is** the fallback tier (WASM or pure JS) — there is no separate fallback construct. A slot without a catch-all simply selects nothing on unmatched platforms, leaving the legacy surface in charge.

#### Selection algorithm (normative sketch)

```
select(candidates, target):
  for candidate in candidates:              # author order
    if candidate has any predicate key this implementation
       does not recognize:                   continue
    if os      present and target.os  ∉ listify(candidate.os):   continue
    if cpu     present and target.cpu ∉ listify(candidate.cpu):  continue
    if libc    present and (target.os ≠ "linux"
                            or target.libc ≠ candidate.libc):    continue
    if napi    present and
       napi_version(target.nodeVersion) < candidate.napi:        continue
    if engines.node present and
       not semver_satisfies(target.nodeVersion, range):          continue
    if candidate manifest os/cpu/libc rejects target:            continue
    return candidate                        # first match wins
  return NONE                               # → legacy surface (see onMissing)
```

Selection never requires downloading a non-selected artifact: predicates live in the parent's manifest, and the artifact's standard `os`/`cpu`/`libc` fields are registry metadata already read while resolving and locking every candidate. Lockfiles must retain those fields so frozen installs can repeat the same validation without fetching tarballs.

An artifact whose own manifest `os`/`cpu`/`libc` fields reject the target is treated as non-matching with a skew warning, and selection continues with the next candidate. Only a candidate that passes both the parent's predicates and its own manifest constraints counts as selected. If none passes, the slot is a miss: `onMissing` applies, no alias or artifact bin is linked, and `supersedesScripts` does not suppress the legacy script. This required validation prevents copy-paste errors and name confusion without stranding users between the artifact and legacy surfaces.

### The `artifacts` field

A new top-level manifest field declaring named **slots**. Each slot is an ordered candidate list. The parent package always installs as itself; the field tells the package manager which additional package to materialize and how to expose it.

```jsonc
{
  "name": "sharp",
  "version": "0.34.0",
  "imports": { "#addon": "_addon" },                  // optional sugar; see below
  "optionalDependencies": {
    "@img/sharp-linux-x64": "0.34.0",
    "@img/sharp-linuxmusl-x64": "0.34.0",
    "@img/sharp-darwin-arm64": "0.34.0",
    "@img/sharp-win32-x64": "0.34.0"
  },
  "artifacts": {
    "addon": {
      "candidates": [
        { "package": "@img/sharp-linux-x64",     "os": "linux",  "cpu": "x64", "libc": "glibc", "napi": 9 },
        { "package": "@img/sharp-linuxmusl-x64", "os": "linux",  "cpu": "x64", "libc": "musl",  "napi": 9 },
        { "package": "@img/sharp-darwin-arm64",  "os": "darwin", "cpu": "arm64", "napi": 9 },
        { "package": "@img/sharp-win32-x64",     "os": "win32",  "cpu": "x64",   "napi": 9 },
        { "package": "@img/sharp-wasm32@0.34.0" }     // predicate-free: the fallback tier
      ],
      "onMissing": "warn"                             // "warn" | "error" | "ignore"
    }
  }
}
```

Field rules:

- **Slot names** match `[a-z0-9-]+` and derive the alias `_<slot>`.
- **Candidate versions**: a bare `package` name takes its exact version from the parent's own `optionalDependencies` (or `dependencies`) entry, which **must** exist and **must** be exact. The `name@version` inline form pins candidates deliberately *not* listed in `optionalDependencies`, so legacy package managers never download them (see the Prisma example below). Ranges are a manifest error in either form.
- **Candidate names should be scoped** (see *Security considerations*).
- **There is no separate fallback construct.** A predicate-free candidate listed last is the fallback tier; omitting one means the slot selects nothing on unmatched platforms and the parent's own JS/bin remains authoritative.
- **`onMissing`** governs behavior when no candidate matches: `"warn"` (default), `"error"` (for packages with no working legacy surface), or `"ignore"` (the miss is expected and the parent handles it — see the Prisma example). Unreachable when the slot ends in a predicate-free candidate.
- **`supersedesScripts`** (optional) lists parent lifecycle events (`preinstall`, `install`, `postinstall`) that exist only as this slot's legacy fallback. When the slot selects and validates a candidate, a conforming package manager **must not** execute them (see *Lifecycle-script supersession*).
- **The parent's top-level `bin` is the command-name authority.** Executable names declared by artifacts (see *The `artifact` field*) are linked only when they appear in the parent's top-level `bin`: legacy installs always have the command, and no platform grows phantom commands.

CLI example:

```jsonc
{
  "name": "esbuild",
  "version": "0.25.0",
  "bin": { "esbuild": "bin/esbuild" },                // legacy JS shim, and the command-name authority
  "artifacts": {
    "cli": {
      "candidates": [
        { "package": "@esbuild/linux-x64",    "os": "linux",  "cpu": "x64" },
        { "package": "@esbuild/darwin-arm64", "os": "darwin", "cpu": "arm64" },
        { "package": "@esbuild/win32-x64",    "os": "win32",  "cpu": "x64" },
        { "package": "esbuild-wasm@0.25.0" }          // fallback tier: WASM build
      ]
    }
  }
}
```

Where each executable lives is not the parent's business: each candidate declares its own path in the artifact-side `artifact` field (next section), which is how the Windows build gets to call its file `esbuild.exe` without the parent maintaining per-candidate overrides.

Binary-payload example (Prisma-style; note the inline pins keeping engines out of legacy installs entirely):

```jsonc
{
  "name": "@prisma/engines",
  "version": "6.5.0",
  "imports": { "#query-engine": "_query-engine", "#query-engine/*": "_query-engine/*" },
  "artifacts": {
    "query-engine": {
      "candidates": [
        { "package": "@prisma/qe-linux-x64-glibc@6.5.0", "os": "linux", "cpu": "x64", "libc": "glibc" },
        { "package": "@prisma/qe-darwin-arm64@6.5.0",    "os": "darwin", "cpu": "arm64" }
      ],
      "onMissing": "ignore"                           // no catch-all: on a miss the parent's
                                                      // existing download-on-demand path remains
    }
  }
}
```

### The `artifact` field

The parent's side of the contract says *which package* to materialize; the artifact's side says *what it contains*. Artifact packages declare their own executables in a new top-level manifest field:

```jsonc
// @esbuild/win32-x64/package.json
{
  "name": "@esbuild/win32-x64",
  "version": "0.25.0",
  "os": ["win32"],
  "cpu": ["x64"],
  "artifact": { "bin": { "esbuild": "esbuild.exe" } }
}
```

- **`artifact.bin`** maps command names to paths inside the package, the same shape as the standard `bin` field. v1 defines only the `bin` key; the field is an object so future revisions can add payload or addon hints without a shape change, and unknown keys inside it are ignored.
- **The standard `bin` field cannot serve this purpose.** Legacy package managers link the bins of every installed package, so an artifact declaring top-level `bin` would fight the parent's same-named JS shim in `.bin` directories, nondeterministically. Platform packages today ship no `bin` for exactly this reason (esbuild's optional postinstall exists to swap the shim into place manually). Artifact packages **must not** declare top-level `bin`; `artifact` is invisible to legacy package managers, so declaring it changes nothing about legacy installs.
- Registries strip unknown fields from abbreviated packuments, so `artifact` is normally visible only in the tarball's own `package.json`. Bin materialization happens after the tarball is on disk, so this costs nothing at install time; it does make bin-name validation a link-time check, and registry-side validation needs the full manifest.
- **A candidate without an `artifact` field is still a valid candidate.** The alias is linked and the parent's top-level `bin` stays in place, where the parent's JS shim resolves the artifact through the alias — the slot still works. An already-published fallback package (`esbuild-wasm`) participates with no republish; adding `artifact.bin` in a later version upgrades it to a direct link.

### The linkage contract

For slot `foo`, the package manager materializes the selected artifact so that the bare specifier **`_foo`** resolves *from the parent package* to the artifact's root:

- **Hoisted layouts** (npm, Yarn classic, Bun): `node_modules/<parent>/node_modules/_foo` → symlink/junction to the artifact directory. Nested `node_modules` is standard resolution; hoisting never applies to it.
- **Isolated layouts** (pnpm, aube): the alias is one more edge in the parent's dependency realm, exactly how those installers already inject dependencies. The content-addressed store is never mutated; the parent package's bytes stay pristine.

The selected artifact is *also* linked under its real name whenever it is a declared optional dependency (unchanged semantics); the alias is purely additive, so code referencing real names keeps working.

Why a `_`-prefixed bare name:

- **Unsquattable**: conforming registries reject package names beginning with `_`, so no registry publish can ever shadow the alias.
- **Zero Node.js changes**: a directory named `_foo` resolves in every Node ever shipped, and in Bun/Deno's node-compat resolution.
- **Behavioral fallback**: under a legacy package manager the directory doesn't exist, so the shipped alias-presence guard chooses the existing loader chain. The fallback needs no configuration — it is the absence of the alias.

The normative loader pattern:

```js
const fs = require('node:fs');
const path = require('node:path');

function hasPackageAlias(name) {
  return (require.resolve.paths(name) ?? []).some((nodeModules) => {
    try {
      fs.lstatSync(path.join(nodeModules, name));
      return true;
    } catch (error) {
      if (error?.code === 'ENOENT') return false;
      throw error;
    }
  });
}

const native = hasPackageAlias('_addon')
  ? require('#addon')               // alias exists: all load errors propagate
  : legacyRequireChain();           // alias absent: use today's fallback
```

The guard tests the `_slot` directory entry itself rather than catching resolution errors. A dangling alias or an artifact with a missing `main`, invalid `exports`, initialization failure, or missing transitive dependency therefore takes the artifact branch and fails visibly; only an actually absent alias activates the legacy loader.

`imports: { "#addon": "_addon" }` is **recommended sugar, not a requirement**: it gives parent code and bundlers a single static `#`-namespaced specifier, is inert in the published tarball, and behaves identically under all package managers. A parent may `require('_addon')` directly.

Payload access needs no new API — plain resolution:

```js
const dir = path.dirname(require.resolve('#query-engine/package.json'));
const engine = path.join(dir, 'query-engine' + (process.platform === 'win32' ? '.exe' : ''));
```

(Artifact packages **should** omit `exports` or export `"./package.json"` so this resolves.)

#### Rejected linkage mechanisms

| Mechanism | Why rejected |
|---|---|
| New resolver condition (e.g. a `platform:linux-x64` condition in `exports`/`imports`) | Requires changes to Node, Bun, and every bundler — a flag day; and conditions select *subpaths*, not *package versions*, so they cannot express "a different tarball per platform" |
| Package manager rewrites the parent's `package.json` (`imports`) at install time | Mutates package bytes: breaks content-addressed store sharing, integrity re-verification, and install idempotence |
| Well-known metadata file dropped into the parent's directory, read at runtime | Same store-mutation problem, plus bespoke runtime resolution code in every package |

### Bin entries

When a selected artifact declares `artifact.bin`:

1. Each entry maps a command name to a path inside the artifact. Names absent from the parent's top-level `bin` are ignored with a warning (the name-authority rule).
2. **Containment check**: the resolved path must not escape the artifact directory after symlink resolution (string containment, then `realpath` containment).
3. The artifact bin **overrides** the parent's same-named top-level `bin` entry in every `.bin` directory the package manager populates.
4. Selected slots must expose disjoint command names. If two selected artifacts declare the same `artifact.bin` name, the package manager must reject the manifest before changing any `.bin` entry; slot or object iteration order never determines the winner.
5. Unix: symlink into `.bin`, or copy the executable and set the copy to mode `0755`. A package manager must not chmod a hardlink to a content-addressed-store inode; hardlinks are permitted only when the stored inode is already executable and no mode change is required. Windows: a native target is a real PE executable — link/copy it as `<name>.exe` and/or emit shims that exec it *directly*, never through the "interpret with node" default. A fallback-tier artifact may declare a JS entry point; the package manager emits an ordinary Node shim for it.
6. If the slot selects nothing, or the selected artifact declares no `artifact.bin`, the parent's top-level `bin` is used untouched — bit-identical to legacy behavior.

### Lifecycle-script supersession

The install-script packages of *Workaround 1* are prime adoption targets, and progressive enhancement forces them to keep their download script for legacy installers. Without a rule for the hit path, a conforming package manager with scripts enabled would run the legacy downloader *and* materialize the selected artifact: the same binary downloaded twice, with the script racing the package manager for the `bin` paths. The script cannot avoid this on its own. `preinstall` runs before anything is materialized, so there is no alias to probe, and probing is itself install-time code execution.

Supersession is therefore **declared, not detected**. A slot lists the parent lifecycle events that exist only as its legacy fallback:

```jsonc
{
  "name": "aube",
  "version": "1.40.0",
  "bin": { "aube": "bin/aube.js" },                   // legacy JS shim
  "scripts": { "preinstall": "node npm/installArchSpecificPackage.js" },
  "artifacts": {
    "cli": {
      "supersedesScripts": ["preinstall"],            // artifact replaces the downloader entirely
      "candidates": [ /* one per platform */ ]
    }
  }
}
```

Rules:

- **When a slot selects and validates a candidate, a conforming package manager must not execute the parent lifecycle events listed in that slot's `supersedesScripts`.** The artifact replaces the script's entire purpose; running both is a bug. A candidate rejected by its own platform metadata is non-matching and cannot supersede a script.
- A superseded script is not *skipped*; it is not part of the install at all. Package managers with build-approval flows (pnpm's `onlyBuiltDependencies`, aube's `approve-builds`) **must not** count a superseded script as pending approval or prompt users to allowlist it, and **must not** emit skipped-lifecycle-script warnings for it. Those warnings exist so users notice a package that may need its script; this one does not.
- Values are restricted to `preinstall`, `install`, and `postinstall`, the events install-fallback scripts actually use. Listing any other event is a manifest error.
- A script listed by several slots is superseded only when **every** listing slot selected a candidate. If any listing slot missed, the script runs under normal script policy: the script is the legacy surface, and the miss makes the legacy surface authoritative. Package managers **should** expose per-slot outcomes to a superseded script that does run (`npm_package_artifacts_<slot>` set to the selected package name, empty on a miss) so it can skip work an artifact already covered.
- On a miss, "normal script policy" includes policies that skip scripts by default (pnpm ≥ 10, aube): the fallback script may still not run until approved, exactly as on a non-conforming install. Supersession changes only when the skipped-script warning appears: on installs where the script actually matters. Package managers **should** fold the miss into that diagnostic, e.g. "no `cli` artifact matched linux/riscv64; fallback script `preinstall` is not approved — run `approve-builds`". `onMissing: "error"` remains the escalation for slots where neither surface may silently fail.
- Listing an event the parent's `scripts` does not define is skew, in the same lint category as predicate/manifest skew; package managers **should** warn.
- On a legacy package manager the field is inert inside the ignored `artifacts` object: the script runs exactly as today. A publisher whose script also does unrelated work must split the script before declaring supersession; the field declares full replacement.

Supersession also gives the build-from-source population a conforming path. Many addons (node-serialport, node-usb, most node-gyp packages) have no WASM tier; their only universal fallback is compilation. That is just another superseded fallback: the slot lists prebuilt candidates for the common platforms and declares the `install` script (`node-gyp rebuild`) superseded, so matched platforms get a prebuilt artifact with zero code execution while a miss falls back to today's source build under normal script policy. The build-from-source tier needs no new construct; it is the legacy surface.

The compat contract is symmetric: on a miss the legacy surface, scripts included, is authoritative; on a hit it is inert (bins shadowed, alias-present branch used, scripts superseded).

### Lockfiles

- Candidates referenced by bare name are locked through their `optionalDependencies` entries — no change from today. Inline-pinned candidates (`name@version`) are locked as additional entries reachable from the parent via a new **artifact edge** (in `package-lock.json` terms: ordinary package entries with optional-equivalent semantics plus an `"artifact": true` marker; edge-modeling lockfiles add an `artifactDependencies` edge type).
- **Selection is never locked** (invariant 6). Frozen installs (`npm ci`, `--frozen-lockfile`) re-run selection against locked versions; a selection miss is not a lockfile mismatch.
- **Unresolvable candidates degrade like optional dependencies.** A declared candidate that cannot be resolved at lock time (typically not yet published: parents and their platform matrices routinely publish from CI minutes apart) is warned about, omitted from the lockfile, and treated as non-matching by selection; remaining candidates and `onMissing` apply as usual. A failed or delayed artifact publish therefore degrades to the legacy surface instead of breaking every install, and a later resolve that finds the candidate locks it then.
- **Multi-tuple installs**: configuration enumerating extra target tuples (pnpm `supportedArchitectures` precedent) causes the package manager to run selection per tuple and fetch each tuple's selected artifact into the cache — and, where the layout permits, materialize them under their real names — while alias and bin links are created for the host tuple only.

### Legacy compatibility

A conforming package publishes both mechanisms simultaneously; each surface degrades independently:

| Component | Legacy package manager | Conforming package manager |
|---|---|---|
| `optionalDependencies` on platform packages (each with its own `os`/`cpu`/`libc` fields) | filtered to the matching one (npm ≥ 10.4 / pnpm); older installers tolerate optional failures | resolved and locked; matching one linked under its real name |
| Top-level `bin` → JS shim | the command | **shadowed** when the selected artifact declares `artifact.bin`; untouched otherwise |
| Runtime loader checks for `_slot`, then loads `#slot` or the legacy chain | alias absent, so the legacy branch runs | alias present, so `require('#slot')` runs and any artifact error propagates |
| `imports: {"#slot": "_slot"}` | inert mapping to a nonexistent name | resolves to the alias |
| `artifacts` field | unknown field, ignored | drives everything |
| `artifact` field (in artifact packages) | unknown field, ignored; artifact packages declare no top-level `bin`, so nothing links | declares the executable paths the package manager links |
| Lifecycle scripts | none needed for the napi-rs pattern; postinstall-download users keep their script as the legacy tier and declare it via `supersedesScripts` | **superseded**: never executed when the declaring slot selects and validates a candidate (see *Lifecycle-script supersession*); never on artifact packages |

**Authority rule (normative)**: the legacy surface is the authoritative fallback. A conforming package manager that selects no artifact for a slot must behave exactly as a non-conforming one for that slot's bins, links, and superseded scripts: a script listed in `supersedesScripts` runs under normal script policy when its slot misses.

Adoption requires no restructuring. Addon packages (sharp, napi-rs output) add the `artifacts` field and a small alias-presence guard to their loader, and ship; their platform packages need no changes at all. CLI packages additionally add `artifact.bin` to each platform package — a one-field diff emitted by the same generator, and exact-pin lockstep means the whole matrix republishes with every release anyway. Generators (napi-rs, esbuild's publish tooling) can emit `optionalDependencies`, candidates, and both manifests from a single target-triple definition.

### Security considerations

- **Exact pins only.** A candidate version is either the exact version in `optionalDependencies` or an inline exact pin; ranges are a manifest error. Native artifacts are ABI-coupled to their parent's JS and built in lockstep; floating versions are a supply-chain and ABI hazard.
- **Scoped candidate names are recommended rather than required** — ideally a scope owned by the parent's publisher (`@esbuild/*`, `@img/*`), which lets registries validate publisher overlap at publish time and package managers warn otherwise. Yarn's variants RFC made scopes a hard requirement to close a squatting hole opened by template-generated names, an open namespace this RFC rejects; here every candidate is a literal, exact-pinned, integrity-locked name the publisher wrote by hand, the same trust surface as any other dependency entry. A hard requirement would also force existing unscoped families (`esbuild-wasm`, aube's `aube-*` matrix) to republish under new names in order to adopt. Package managers **should** warn on unscoped candidates. Artifact packages **should** carry npm provenance attestations so auditors can verify parent and artifacts were built from the same source.
- **Integrity**: candidates are ordinary locked packages — SHA-512 from the lockfile, verified from cache/mirror/offline like anything else. No new trust surface.
- **No lifecycle scripts on artifacts** (invariant 2), codifying the `--ignore-scripts` hardening posture.
- **Script supersession closes the double-execution hole.** Without it, an adopting package that keeps a download script for legacy installers would still execute code at install time on conforming package managers unless users disable scripts. With `supersedesScripts`, the conforming hit path executes zero package code even with scripts fully enabled; the hardening no longer depends on user configuration.
- **Overrides/resolutions** apply as the user's escape hatch (e.g. patching a vulnerable artifact), but the package manager **must** warn when an override moves an artifact off its declared exact version.
- **Alias squatting is impossible** on conforming registries (`_` prefix unpublishable), and the alias lives in the parent's nested realm, which shadows any hoisted name.
- **Path containment** for bin materialization (*Bin entries*, step 2).
- **Command names are parent-authoritative.** Only names present in the parent's top-level `bin` are ever linked, so a compromised artifact can redirect an existing command but cannot introduce new ones. The executable-path claim itself ships inside the artifact's own integrity-hashed, provenance-attested tarball, next to the executable it describes.

### Known costs

1. **The parent stays fat.** Every consumer downloads the legacy JS shim, loader chain, and possibly a never-executed fallback tier, until a publisher decides its user base has migrated and drops them. Progressive enhancement *is* the bloat.
2. **The matrix is declared three times**: `optionalDependencies`, `artifacts` candidates with predicates, and each artifact's own `os`/`cpu`/`libc` manifest fields. Skew between them is a new lint category, mitigated by generators emitting all three from one definition and by the selection-time cross-check, but real verbosity.
3. **Two code paths during the transition.** The alias-present branch is live on conforming package managers and the alias-absent branch on legacy ones; whichever branch CI doesn't exercise rots. (This cost is shared by any progressive-enhancement design.)
4. **A novel on-disk shape.** `_slot` directories will confuse `npm ls` (extraneous), license scanners, and serverless/Electron packagers until tooling learns the convention, though a static `#slot` specifier is strictly more analyzable than today's dynamic `require(namesByPlatform[key])`.
5. **Hoisted-npm implementation friction.** Arborist must model an alias edge that corresponds to no dependency range, survive reify/prune cycles, and not report it extraneous. Isolated-layout installers (pnpm, aube) get the alias nearly free; npm does not — and npm's buy-in is the adoption bottleneck.

## Rationale and Alternatives

### Alternative considered: whole-package substitution

*Lineage: npm RFC [#519 "Package Distributions"](https://github.com/npm/rfcs/pull/519), Yarn [Package Variants](https://github.com/yarnpkg/berry/issues/2751).*

The strongest alternative, developed in full during the drafting of this RFC, inverts the mechanism: instead of the parent staying and an artifact being exposed *to* it, the parent declares an ordered list of **variant packages** and the selected variant is reified **in place of the parent, under the parent's name**. If none matches, the parent itself reifies — fallback is structural rather than behavioral.

Sketch:

```jsonc
{
  "name": "esbuild",
  "version": "0.25.0",
  "bin": { "esbuild": "bin/esbuild-shim.js" },       // legacy surface
  "variants": {
    "select": [
      { "os": "darwin", "cpu": "arm64",                  "package": "@esbuild/darwin-arm64" },
      { "os": "linux",  "cpu": "x64",   "libc": "glibc", "package": "@esbuild/linux-x64" },
      {                                                  "package": "@esbuild/wasm" }
    ],
    "onMiss": "fallback"
  }
}
```

A variant's version is implicitly the parent's exact version (parity by construction). The selected variant's manifest governs everything at the parent's tree position — `main`, `exports`, `bin`, `dependencies` — and `require("esbuild")` resolves into it from anywhere. Because a variant must be a *complete, runnable* package, the addon case (bcrypt/sharp) requires extracting the parent's JavaScript into a shared `-core` package that every variant depends on, each variant shipping a generated entry file binding core to its local `.node` file.

Genuine advantages over the proposed design:

- **The parent name itself is the stable alias** — `require.resolve("@prisma/engines/artifacts/query-engine")` works from *anywhere* in the tree, not just from inside the parent.
- **Thin installs**: consumers never download the legacy shim/loader at all; the selected variant is all there is.
- **No triple declaration** and no novel `_slot` shape — variants are plain packages at plain paths.
- For **pure-binary CLI packages** it is the cleanest conceivable shape.

Why it was not chosen:

1. **Migration is restructuring, not annotation.** Every existing addon package must extract a `-core`, add entry files to every platform tarball, and republish its whole matrix. The proposed design lets sharp adopt with a field addition and a small loader guard while reusing packages already published. Standards that require the ecosystem to restructure historically stall (this RFC reads #519's fate partly that way); standards that annotate existing practice ship (npm 10.4's `libc`).
2. **On-disk identity mismatch.** `node_modules/esbuild/package.json` would say `"name": "@esbuild/darwin-arm64"`. Every scanner, bundler heuristic, jest module mapper, and `patch-package` user encounters that novelty — tools that never opted into the standard pay for it. It cannot be papered over without rewriting the variant's manifest at link time, which would break integrity verification and content-addressed store sharing. The proposed design's failure mode, by contrast, is the status quo: no alias, catch path, today's behavior.
3. **One variant per tree position.** Materializing artifacts for several platforms into one `node_modules` (pnpm `supportedArchitectures`, mac-host/linux-container volume mounts) is structurally impossible under substitution. The proposed design lets all candidates coexist under their real names, leaving only the alias host-bound.
4. **Version-parity rigidity.** A one-platform binary hotfix requires a new parent version and re-tagging every variant; independently-versioned artifacts (Prisma's engines) fit poorly. The proposed design's inline pins allow either regime.
5. **Behavioral parity across N+1 packages is enforced by discipline, not construction** — a variant whose JS drifts from core is a platform-specific behavior fork. Under the proposed design there is exactly one copy of the parent's JS.

Substitution remains worth pursuing if the identity-mismatch problem finds a principled solution (for example, registries treating variants as first-class "faces" of a parent package). Nothing in the proposed design precludes adding it later: the selection primitive — predicates, ordering, all-locked/one-materialized — is deliberately shared, and a future `variants` field could reuse it verbatim. See *Unresolved Questions*.

Summary comparison, extended to the two prior proposals the substitution model descends from:

| Dimension | Proposed (`artifacts`) | Substitution (`variants`, as refined here) | npm #519 (`distributions`) | Yarn #2751 (Package Variants) |
|---|---|---|---|---|
| Core move | selected artifact exposed *to* the parent at `_slot` | selected variant reified *as* the parent | variant reified as the parent (Arborist Link) | variant substituted at resolution, alias-style |
| Pure-binary CLI | bin override → real executable | cleanest: variant *is* the package | `bin` handling unspecified (asked in review, unanswered) | `bin` handling unspecified |
| Addon library (bcrypt, sharp, @swc) | **additive**: add field + alias-presence guard | restructure into `-core` + republish matrix | same restructuring implied | same restructuring implied |
| Stable name for third parties | `_slot`, parent-internal by design | parent name, tree-wide | parent name, tree-wide | parent name, tree-wide |
| On-disk identity | standard shapes + one novel link | directory name ≠ manifest name | directory name ≠ manifest name | same mismatch on `node_modules` linkers; virtualized away under PnP |
| Candidate naming | literal lists; scopes recommended | literal lists; scoped | literal specifiers | template-generated (`%platform-%napi`); scopes required against squatting |
| Version discipline | exact pins; inline pins allow independent versioning | parity with parent by construction | semver ranges (`@1.x`) | parity with parent by construction |
| Selection language | `os`/`cpu`/`libc`/`napi`/`engines.node`; first-match; unknown-key skip rule | same primitive (shared) | `platform`/`arch`/`engines`; no libc; matching semantics unspecified | freeform parameter matrix; consumer-extensible via `dependenciesMeta` |
| Artifact/variant dependencies | ordinary locked optional deps | variant's deps govern at the parent's position | must not differ across distributions (its own stated known risk) | unresolved |
| Lifecycle scripts | superseded declaratively (`supersedesScripts`) | — | replacing them is motivation, no mechanism | not addressed |
| Multi-platform `node_modules` | candidates coexist under real names | structurally impossible | single reify position | single position; `supportedArchitectures` fetches only |
| Failure mode | status quo (legacy path) | novel breakage surface | parent reifies (structural fallback) | parent reifies (field ignored) |
| npm implementability | new alias-edge bookkeeping in Arborist | reify-as-link designed in #519 | designed for Arborist, never built | PnP resolver-table native; Arborist story untested |
| Scope | binary artifacts only | binary artifacts only | also polyfills, full/slim, ESM/CJS contemplated | also ESM/CJS, source, docs, locales via custom parameters |
| Status | this proposal | alternative developed herein | closed unmerged (2023, repository cleanup) | open, dormant |

### Other alternatives

- **Do nothing.** The status quo works, in the sense that install scripts and try/catch loaders exist. But the costs enumerated in *Motivation* are paid by every user of every native package on every install and every invocation, and the security posture of install scripts worsens as supply-chain attacks increase. The ecosystem is already converging on the artifact-package half of this design; this RFC standardizes the selection half that every project currently reinvents.
- **Document the existing pattern instead of standardizing a field.** A "best practices" document would not remove the runtime loader, the CLI shim tax, or the hand-rolled libc detection — those exist precisely because there is no install-time selection contract.
- **New resolver conditions in Node.js.** Elegant for the addon case, but requires coordinated Node/Bun/bundler changes (a multi-year flag day), cannot select different *packages* (only subpaths within one package), and does nothing for `bin`.
- **Per-package-manager proprietary mechanisms.** Bun's and others' special-casing of known packages proves demand but does not scale past a hard-coded list, and publishers cannot target it.

A manifest field plus specified package-manager behavior is the minimal standard: it changes nothing in Node, nothing in the registry protocol, and nothing for legacy installers.

## Implementation

Feasibility notes per package manager:

- **npm**: the alias is a new edge concept in Arborist's ideal/actual trees (an edge with no dependency range that must survive reify/prune and not report extraneous) — the largest single implementation lift in this proposal, called out honestly under *Known costs*. npm ≥ 10.4 already implements the `libc` matching semantics this RFC reuses.
- **pnpm / aube** (isolated layouts): the alias is one more link in a dependency realm — machinery both installers already have. pnpm's `supportedArchitectures` is the multi-tuple precedent.
- **Yarn**: Plug'n'Play resolution is virtualized, making the alias a resolver-table entry rather than a filesystem link; `node_modules` linkers behave like npm's case.
- **Bun**: performs install-time platform filtering already; the alias and direct-exec bin links fit its linker.

As concrete evidence of implementability, **aube** (a Rust package manager with npm-compatible behavior) already contains every building block: npm-semantics `os`/`cpu`/`libc` matching and graph filtering, runtime glibc/musl detection with the probe-order hardening described above, bin shim creation that detects native-executable magic and execs directly (including Windows `.cmd`/`.ps1`/sh emission), and per-platform artifact-variant selection in its lockfile layer (used today for Node.js runtime pins). The author intends aube to serve as the reference implementation, and aube's own npm distribution — which today requires a `preinstall` download script — as the dogfood target: the script stays only for legacy installers, declared via `supersedesScripts`, so a conforming install executes no code at all and links `bin` straight to the native binary.

Registry-side work is optional but valuable: publish-time validation (scoped candidate names, exact-pin rules, bin-name subset rules) and provenance linkage between parent and artifact packages.

## Prior Art

- **npm RFC [#519 — Package Distributions](https://github.com/npm/rfcs/pull/519)** (2022; closed unmerged 2023). Proposed `distributions: [{platform, arch, engines, package}]` with all-locked/one-reified semantics and implicit fallback to the original package — the substitution alternative above is its direct descendant. Closed in repository cleanup rather than rejected on the merits. This RFC narrows scope to binary artifacts (where #519 also contemplated ESM/CJS builds, docs/test slimming, and general variants), adds the explicit selection primitive, libc, and the no-shim requirement, and — for the reasons given under *Rationale* — keeps the parent package in place rather than substituting it.
- **Yarn [Package Variants RFC](https://github.com/yarnpkg/berry/issues/2751)** (open). Pattern + matrix name templating with parameter cascading. This RFC borrows its exact-version discipline, non-Turing-completeness goal, and graceful-degradation stance, relaxes its scoped-name requirement to a recommendation (see *Security considerations*), and rejects name templating (literal candidate lists are greppable and provenance-attestable; no generated-name squatting surface) and consumer-driven parameters (out of scope for v1).
- **npm RFC [#438](https://github.com/npm/rfcs/issues/438) → npm 10.4 `libc` support** (shipped 2024). Proof that incremental, narrowly-scoped platform-selection improvements can land in npm; this RFC reuses its field semantics verbatim — and its shipped-because-it-annotated character informs the choice of the additive design.
- **pnpm [`supportedArchitectures`](https://pnpm.io/settings#supportedarchitectures)** (shipped). Multi-tuple fetching precedent; adopted here as the model for cross-platform cache warming.
- **node-pre-gyp / prebuild-install / prebuildify / napi-rs**. The current practice this RFC formalizes. napi-rs's code generation is the natural emitter of the new field — one target-triple definition can generate `optionalDependencies`, candidates, and artifact manifests together, eliminating the triple-declaration skew risk in practice.
- **npm/npm [#1891 `platformBinaries`](https://github.com/npm/npm/issues/1891)** (2011). The same problem statement, fifteen years ago.
- **npm RFC 0055 — package manifest extensions** (implemented 2026). Recent precedent that new manifest fields still land through npm/rfcs; notably it excluded binary/platform fields from its scope, which supports giving them a dedicated proposal.
- **OpenJS Package Metadata Interoperability WG — `devEngines`**. Demonstrates the intended venue can carry a manifest field from proposal to cross-package-manager implementation (npm and pnpm ship it).

## Unresolved Questions and Bikeshedding

1. Field naming: `artifacts` vs. something else; the artifact-side field name (`artifact` is one letter from `artifacts` — `provides`?); slot-alias prefix (`_slot` vs. another unpublishable namespace); `onMissing` value names; `supersedesScripts` naming.
2. Should the substitution model (*Rationale and Alternatives*) additionally be standardized — now or later — for the pure-binary CLI case where it is cleanest, given the shared selection primitive makes it a compatible extension? The author's position: not in v1; one model keeps five implementations honest.
3. `cpuFeatures` opt-in design for a future revision (explicit "I accept install-machine detection" flag? runtime dispatch guidance?).
4. Minimum glibc version expression (`libc: "glibc"` says nothing about `GLIBC_2.28` symbols; is `engines`-style versioning of libc worth the complexity?).
5. Windows ARM64 x64-emulation and macOS Rosetta: should a publisher express "prefer native, accept emulated" beyond candidate ordering?
6. Universal/fat binaries (macOS `universal2`): a predicate value, or just a candidate listing both `cpu` values?
7. Electron/alternate-ABI targeting: `napi` covers N-API addons; NAN-style ABI-specific builds are deliberately out of scope — confirm.
8. Multi-platform materialization of the *alias* (as opposed to candidates under real names): is there any sound design for volume-mounted `node_modules` crossing os/libc boundaries, or is that formally unsupported?
9. Registry enforcement: which publish-time validations (scoped names, exact pins, bin-name subset) should be normative for registries vs. left to package managers and linters?
10. Should the lockfile record the *candidate table* (for frozen-install auditability) even though it never records the *selection*?

## Feedback

This draft is intended for the [OpenJS Package Metadata Interoperability Working Group](https://github.com/openjs-foundation/package-metadata-interoperability-working-group), with npm/rfcs as the follow-on venue for npm-CLI-specific implementation details. Until it is submitted there, discussion is welcome on [aube's GitHub Discussions](https://github.com/jdx/aube/discussions).
