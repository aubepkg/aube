---
name: soak
description: Manage the repo's supply-chain soak window (SOAK_DAYS) — check/fix the derived surfaces, bump or opt out of the window, add a per-package exclusion with its dated annotation, or bump a pinned external tool. Use whenever a task touches minimumReleaseAge, min-release-age, min-publish-age, the nightly toolchain pin, external-tools.json, taze cooldowns, or a "why won't this fresh version install" question.
---

# The soak window

One rule: a release must be at least `SOAK_DAYS` old before this repo adopts
it. The window is defined exactly once — `scripts/soak/constants.mts`
(`SOAK_DAYS = 7`) — and every surface derives from or is parity-checked
against it:

| Surface | Key | Units |
|---|---|---|
| `.cargo/config.toml` | `global-min-publish-age` | `"N days"` |
| `rust-toolchain.toml` | nightly channel date vs `# adopted:` line | days |
| `docs/pnpm-workspace.yaml` | `minimumReleaseAge` | minutes |
| `docs/.npmrc` | `min-release-age` | days |
| `docs/taze.config.mts` | `maturityPeriod` | imports `SOAK_DAYS` |
| `external-tools.json` | `soakBypass` annotations | days |

## Commands (mise tasks — the scripts live in `scripts/soak/`)

- `mise run soak` — parity-check every surface (CI-gated in test-linux)
- `mise run soak:fix` — rewrite drifted windows, prune expired excludes
- `mise run deps:update` — bump npm (taze) + cargo deps through the window
- `mise run tools:check` / `tools:install` — validate / install the
  SRI-pinned external tools (`external-tools.json`)
- `mise run test:scripts` — the scripts' own unit tests

## Change the window (one place)

1. Edit `SOAK_DAYS` in `scripts/soak/constants.mts`.
2. `mise run soak:fix` (rewrites cargo/npmrc/yaml; taze follows by import).
3. `mise run soak` + `mise run test:scripts` — existing exclude annotations
   encode the old window and will be flagged; re-date or remove them.

**Opt out entirely**: set `SOAK_DAYS = 0` and run the same two steps —
cargo, pnpm/aube (`minimumReleaseAge: 0`), npm, and taze all treat zero as
disabled. There is deliberately no env-var bypass: opting out is a
committed, reviewable change, never a silent one.

## Skip the soak for ONE package (dated, temporary)

Add to `minimumReleaseAgeExclude` in `docs/pnpm-workspace.yaml` with the
annotation on the line above (block list only — flow `[..]` is rejected):

```yaml
# published: 2026-07-08 | removable: 2026-07-15
- 'name@1.2.3'
```

`removable` = `published + SOAK_DAYS`. `published` must be the real registry
publish date. Once `removable` passes, `mise run soak` fails until the pin
is pruned (`soak:fix` does it). Bare names / `@scope/*` globs are standing
trust and need no annotation. External tools use the same shape via a
`soakBypass` object in `external-tools.json`.

## Bump the nightly toolchain

Pick the newest nightly at least `SOAK_DAYS` old **today**, set it as
`channel`, and update the `# adopted: <today>` line in
`rust-toolchain.toml` — `mise run soak` enforces the arithmetic. Also bump
the pre-baked toolchain in `.buildkite/linux-agent.Dockerfile`
(`mise run tools:check` fails on any drift between the image and the
tracked pins).
