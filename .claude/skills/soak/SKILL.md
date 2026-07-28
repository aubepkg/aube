---
name: soak
description: Manages the repo's supply-chain soak window (SOAK_DAYS) — checks and fixes the derived surfaces, bumps or disables the window, adds dated per-package exclusions, bumps pinned external tools, and enforces the Ubuntu-PPA MSRV ceiling. Use when a task touches minimumReleaseAge, min-release-age, min-publish-age, rust-version / MSRV, external-tools.json, sfw shims, renovate.json, or taze cooldowns, or when investigating why a freshly published version won't install.
---

# The soak window

One rule: a release must be at least `SOAK_DAYS` old before this repo
adopts it. The delay gives the ecosystem time to catch a malicious or
yanked release before we ever install it. The window is defined exactly
once — read the current value from `scripts/soak/constants.mts` and never
hardcode it elsewhere. Every surface derives from or is parity-checked
against it:

| Surface | Key | Units |
|---|---|---|
| `.cargo/config.toml` | `global-min-publish-age` | `"N days"` |
| `docs/pnpm-workspace.yaml` | `minimumReleaseAge` | minutes |
| `docs/.npmrc` | `min-release-age` | days |
| `docs/taze.config.mts` | `maturityPeriod` | imports `SOAK_DAYS` |
| `external-tools.json` | `soakBypass` annotations | days |
| `.github/renovate.json` | `minimumReleaseAge` (explicit — an `extends:` preset doesn't count) | `"N days"` |
| `Cargo.toml` | `rust-version` ≤ the PPA rustc ceiling (not window-derived — see below) | version |

## Commands (mise tasks — the code lives in `scripts/soak/`)

- `mise run soak` — parity-check every surface (CI-gated, always-run)
- `mise run soak:fix` — rewrite drifted windows, prune expired exclusions
- `mise run deps:update` — bump npm (taze) + cargo deps through the window
- `mise run tools:check` / `tools:fix` / `tools:install` — validate /
  prune-expired-bypasses / install the SRI-pinned external tools
  (`external-tools.json`); `tools:install` also writes the sfw firewall
  shims into the dev-tools bin dir
- `mise run test:scripts` — the scripts' own unit tests

A soak change is done when `mise run soak` and `mise run test:scripts`
both exit 0 — the same gates CI runs. Re-run them after every fix.

The gates fail closed on invalid states (missing/malformed/wrong-math
annotations) and WARN on expired ones — stale is not unsafe, and nobody
has to watch for it: the scheduled `soak-autofix` workflow runs
`soak:fix` + `tools:fix` daily and commits the pruning as a bot PR.

## The MSRV ceiling (the #1039 guard)

The Ubuntu PPA builds aube's source package on Launchpad with the DISTRO
rustc. The first soak landing (#1020) bumped the workspace MSRV past it
(1.95 > 1.93) and the whole port was reverted (#1039) to unbreak the
release. `mise run soak` now fails any `Cargo.toml` `rust-version` above
`PPA_RUST_CEILING` (`scripts/soak/paths.mts`, with a dated `checked:`
note). Raise the ceiling ONLY after confirming Launchpad's rustc moved:
https://launchpad.net/ubuntu/+source/rustc.

## The cargo soak needs nightly — the repo still must not pin one

`min-publish-age` is an `[unstable]` cargo feature: a stable cargo ignores
it silently. The repo deliberately ships **no** `rust-toolchain.toml`,
because a repo-root toolchain file outranks `rustup default` and would
silently redirect the version-pinned CI legs and the PPA/release builds.
The nightly is instead requested per-invocation, at the only step that
picks versions: `scripts/soak/update-deps.mts` runs `cargo +nightly
update`. Everything else — every CI job, every shipped binary — builds on
stable. If you need the cargo soak somewhere new, call `cargo +nightly`
there; do not add a toolchain file.

## Change the window (one place)

1. Edit `SOAK_DAYS` in `scripts/soak/constants.mts`.
2. `mise run soak:fix` (rewrites cargo/npmrc/yaml/renovate; taze follows
   by import).
3. `mise run soak` + `mise run test:scripts` — existing exclusion
   annotations encode the old window and will be flagged; re-date or
   remove them, then re-run until both pass.

**Opt out entirely**: set `SOAK_DAYS = 0` and run the same two steps —
cargo, pnpm/aube (`minimumReleaseAge: 0`), npm, and taze all treat zero
as disabled. There is deliberately no env-var bypass: opting out is a
committed, reviewable change, never a silent one.

## Skip the soak for ONE package (dated, temporary)

Add to `minimumReleaseAgeExclude` in `docs/pnpm-workspace.yaml` with the
annotation on the line above (block list only — flow `[..]` is rejected
because a comment line can't attach to an inline entry):

```yaml
# published: YYYY-MM-DD | removable: YYYY-MM-DD
- 'name@1.2.3'
```

`removable` = `published + SOAK_DAYS`; `published` must be the real
registry publish date (the placeholders above are schematic — copying
them verbatim is rejected). Once `removable` passes, `mise run soak`
warns until the pin is pruned (`soak:fix` or the soak-autofix workflow
does it). Bare names / `@scope/*` globs are standing trust and need no
annotation. External tools use the same shape via a `soakBypass` object
in `external-tools.json`.

## Maintaining this skill

`scripts/soak/` is the law; this file only documents it — when they
disagree, fix this file. When editing, follow Anthropic's guidance:

- [Prompting best practices](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/claude-prompting-best-practices)
- [Prompting Claude Fable 5](https://platform.claude.com/docs/en/build-with-claude/prompt-engineering/prompting-claude-fable-5)
- [Skill authoring best practices](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices)
- [Write an effective AGENTS.md](https://code.claude.com/docs/en/best-practices#write-an-effective-claude-md)

Keep it concise (goal + constraints, not step enumeration), keep the
description in third person with explicit "use when" triggers, and keep
the window value in `constants.mts` rather than restating it here.
