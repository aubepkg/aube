---
name: aube
description: Manage dependencies, run scripts, and diagnose installs in Node.js projects using aube, aubr, or aubx. Use when the project already uses aube or the user asks to adopt it.
---

# Use aube in a project

Check `aube --version`, the project's `package.json`, lockfile, workspace YAML,
and `.npmrc` before changing dependencies. Preserve the project's package manager
choice and existing configuration unless migration is part of the task.

## Keep the existing lockfile and workspace

aube reads and updates supported pnpm, npm, Yarn, and Bun text lockfiles in place.
It selects the first existing file in this order: `aube-lock.yaml`,
`pnpm-lock.yaml`, `bun.lock`, `yarn.lock`, `npm-shrinkwrap.json`, `package-lock.json`.
Do not delete or import a working lockfile just to use aube: `aube import` creates
an `aube-lock.yaml` that takes precedence and is an intentional format migration.

Use `aube-workspace.yaml` when present; otherwise preserve an existing
`pnpm-workspace.yaml`. Creating an aube workspace file alongside the pnpm file
changes which configuration wins. Preserve `workspace:` and `catalog:` dependency
specifiers when updating packages that use them.

## Choose the command by its effects

| Task | Command |
| --- | --- |
| Install and update a stale lockfile | `aube install` |
| Install without changing a committed lockfile | `aube install --frozen-lockfile` |
| Clean CI install, removing existing `node_modules` | `aube ci` |
| Update only the lockfile | `aube install --lockfile-only` |
| Add a runtime or development dependency | `aube add <package>` / `aube add -D <package>` |
| Update one dependency within its current range | `aube update <package>` |
| Inspect why a dependency is installed | `aube why <package>` |
| Run a project script | `aubr build` / `aube run build` |
| Run an installed binary | `aube exec tsc -- --noEmit` |

`aube update --latest <package>` can move beyond the current range and rewrites the
manifest; use it when that upgrade is intended. Review the manifest and lockfile
diffs together and run the affected project's checks.

`aubr` is `aube run`; it prefers a package script, then a local binary. Script and
exec commands automatically install missing or stale dependencies. After an
explicit install, `aube run --no-install test` skips that install check. Put aube
options before the script name; later arguments are forwarded to the script.

`aubx` is `aube dlx`; it uses a matching local binary first, then a throwaway
installation. `aubx --package <package> <binary>` forces a separate installation.
Prefer a project's existing script or binary when the task needs its pinned
toolchain.

In a workspace, install from the root and scope work with a quoted filter:

```sh
aube -F '@acme/api' run test
aube -F '@acme/api' add zod
aube -r run build
```

`-r` runs across workspace packages; recursive builds use dependency order by
default. Keep a targeted change scoped to the intended package.

## Diagnose installs and dependency builds

Read the first `ERR_AUBE_*` or `WARN_AUBE_*` diagnostic and use `aube doctor` for
environment details. For settings, `aube config find <words>` locates relevant
keys and `aube config explain <key>` describes their sources. Use project-scoped
configuration for a project fix; `aube config set` otherwise defaults to user
scope. Keep registry tokens in environment variables referenced by `.npmrc`.

If a frozen install rejects manifest drift, update the lockfile with
`aube install` only when the manifest change is intended. Do not remove frozen
mode from CI to hide the mismatch.

Root lifecycle scripts run unless scripts are ignored. Dependency scripts need
project approval or built-in trust; explicit denies win. For a missing native
build, inspect `aube ignored-builds` and the package's scripts, approve only the
reviewed package with `aube approve-builds <package>`, then `aube rebuild`. Include
the resulting `allowBuilds` policy in the project change. Do not approve every
build or disable security checks just to clear an install failure.

The default isolated layout does not expose every transitive dependency at the
project root. Check `aube why <package>` and the importing package's manifest
before changing the linker or deleting stores.

For less common flags and compatibility details, prefer the installed command's
`--help`, then the [workflow guides](https://aube.sh/guide) and
[troubleshooting](https://aube.sh/troubleshooting).
