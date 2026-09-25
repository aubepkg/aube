---
description: Add, remove, inspect, update, deduplicate, and prune dependencies with aube.
---

# Manage dependencies

Use `add`, `remove`, `update`, `dedupe`, and `prune` to change a project's
dependency graph.

## Add

```sh
aube add react
aube add -D vitest
aube add -O fsevents
aube add -E typescript
aube add --save-peer react
aube add -g cowsay
```

`add` writes to the correct dependency section, updates the lockfile, fetches
packages into the store, and relinks `node_modules`.

Dependency specifiers can use npm aliases, ranges, dist-tags, workspace
protocols, JSR packages, local directories, tarballs, git URLs, and direct
tarball URLs:

```sh
aube add react@latest
aube add alias-name@npm:actual-name@^1
aube add jsr:@std/collections@^1.0.0
aube add '@acme/ui@workspace:*'
aube add file:../local-package
aube add link:../linked-package
aube add https://registry.example.test/pkg/-/pkg-1.0.0.tgz
```

`jsr:@scope/name` specifiers resolve against JSR's npm-compat endpoint at
<https://npm.jsr.io>. aube registers the `@jsr` scope for you, so no
`.npmrc` setup is needed — the install fetches the package under its
compat name (`@jsr/<scope>__<name>`) and writes `jsr:<range>` back to
`package.json`.

## Inspect before changing

```sh
aube outdated
aube why react
aube list --depth 0
```

Use `outdated` to compare installed and available versions, and `why` to find
which dependency introduced a package. In a workspace, add
`--filter @acme/app` to scope a supported command.

## Remove

```sh
aube remove react
aube remove -g cowsay
```

`remove` updates the manifest and relinks the install.

## Update

```sh
aube update
aube update react
aube update --latest react
```

`--latest` updates past the current manifest range and rewrites the manifest
specifier to the resolved version.

`aube update --interactive` (`-i`) lists every dependency with a newer version
and lets you choose, per package, between staying put, the newest version its
range allows, and the registry's `latest`:

```
Choose which dependencies to update
              Current     Range      Latest
 > chalk      [•] ^4.1.2             [ ] ^6.0.0
   is-number  [•] ^6.0.0             [ ] ^7.0.0
   ms         [ ] 2.0.0   [•] 2.1.3
   semver     [ ] 7.5.0   [•] 7.8.5
↑/↓/k/j up/down • ←/→/h/l choose • / filter • enter confirm
```

The manifest keeps each specifier's shape: `^4.1.2` becomes `^6.0.0`, and an
exact pin such as `7.5.0` is offered the newest release its caret range allows,
then stays an exact pin (`7.8.5`).

## Dedupe

```sh
aube dedupe
aube dedupe --check
```

`dedupe` re-resolves the lockfile to collapse duplicate versions where ranges
allow it. `--check` exits non-zero when the lockfile would change.

## Prune

```sh
aube prune
aube prune --prod
aube prune --no-optional
```

`prune` removes extraneous packages from `node_modules`, including stale
virtual-store entries and dangling `.bin` links.

It reads the lockfile to decide what should remain installed, but it does not
modify `package.json` or the lockfile. Use `aube store prune` instead when you
want to clean unreferenced files from the global store printed by
`aube store path`.
