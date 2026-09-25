---
description: Use committed lockfiles, choose cache directories, and prepare production installs with aube in CI and containers.
---

# CI and containers

Commit the project's lockfile and build approvals before adding aube to CI.
A frozen install fails when the manifest and lockfile disagree, making the
failure visible instead of updating dependency versions during a build.

## Choose an install command

| Command | Existing `node_modules` | Lockfile behavior |
| --- | --- | --- |
| `aube ci` | Removed before installation | Requires a fresh committed lockfile |
| `aube install --frozen-lockfile` | Can reuse the current install | Requires a fresh committed lockfile |
| `aube install --prod --frozen-lockfile` | Installs production dependencies | Requires a fresh committed lockfile |

Use `aube ci` for a clean build. Use `--frozen-lockfile` when retaining an
existing install is useful. Set the flag explicitly in scripts so the intent
is clear outside CI too.

## GitHub Actions

The [aube setup action](https://github.com/jdx/aube-action) installs the native
binary and can install Node.js in the same step:

```yaml
name: Test
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: jdx/aube-action@v1
        with:
          node-version: "24"
      - run: aube ci
      - run: aube run --no-install test
```

Use the Node version required by your project. The final command skips the
auto-install check because the preceding step already installed dependencies.
See the action's README for version pins, inputs, and outputs.

## Dependency builds

CI cannot make an interactive build-approval decision. Review required scripts
locally with `aube ignored-builds` and `aube approve-builds`, then commit the
resulting workspace YAML. `strictDepBuilds: true` makes unreviewed dependency
builds fail installation instead of being skipped.

For stricter policy, see the [security settings](/security#the-paranoid-switch).
If you enable jailed builds, check the runner's
[platform requirements](/package-manager/jailed-builds#native-enforcement).

## Cache choices

`aube store path` prints the resolved content store, including its `v1/`
directory. Registry metadata lives separately under the configured cache
directory. `aube doctor` shows the resolved paths.

The [global virtual store](/package-manager/global-virtual-store) is disabled
in CI by default, so `node_modules` holds real package files and can be cached
like any other directory. A frozen lockfile still controls which versions are
installed. A cache is an optimization, not a replacement for the committed
lockfile or build policy.

Pick one of two layers to cache:

| Cache | Install command | Restored job |
| --- | --- | --- |
| `node_modules` | `aube install --frozen-lockfile` | Reports "Already up to date" without downloading or linking |
| Content store | `aube ci` | Links every package again, but skips tarball downloads |

`aube ci` deletes `node_modules` before installing, so a restored
`node_modules` cache does nothing with it. Pair a `node_modules` cache with
`aube install --frozen-lockfile`.

### Cache `node_modules`

```yaml
steps:
  - uses: actions/checkout@v7
  - uses: jdx/aube-action@v1
    id: aube
    with:
      node-version: "24"
  - uses: actions/cache@v6
    with:
      path: node_modules
      key: aube-nm-${{ runner.os }}-${{ runner.arch }}-node${{ steps.aube.outputs.node-version }}-${{ hashFiles('aube-lock.yaml') }}
  - run: aube install --frozen-lockfile
  - run: aube run --no-install test
```

Include the Node.js version and runner architecture in the key. Dependency
builds approved in `allowBuilds` can compile native addons for one Node.js ABI,
and aube does not reinstall a restored `node_modules` when only the Node.js
version changes. Don't add a `restore-keys` fallback for this cache: a partial
match would be treated as the installed state. In a workspace, add each
package's `node_modules` directory to `path`.

### Cache the content store

```yaml
steps:
  - uses: actions/checkout@v7
  - uses: jdx/aube-action@v1
  - id: aube-store
    shell: bash
    run: echo "path=$(aube store path)" >> "$GITHUB_OUTPUT"
  - uses: actions/cache@v6
    with:
      path: ${{ steps.aube-store.outputs.path }}
      key: aube-store-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('aube-lock.yaml') }}
      restore-keys: aube-store-${{ runner.os }}-${{ runner.arch }}-
  - run: aube ci
```

The content store is independent of the Node.js version, so matrix jobs can
share it. The `restore-keys` fallback is safe here because the store is
content-addressed and packages are looked up by the lockfile's integrity
hashes. Each lockfile change carries older packages forward, so the cache
grows over time.

## Container builds

Install aube in the image using one of the [installation methods](/installation),
then copy dependency inputs before application source when arranging cacheable
layers. Include the lockfile, `package.json`, workspace manifests, patches, and
configuration that affect resolution.

```sh
# Build stage: include development tools.
aube install --frozen-lockfile
aube run --no-install build

# Runtime stage: install only runtime dependencies.
aube install --prod --frozen-lockfile
```

Run the commands in their respective stages; they are not a complete Dockerfile.
Root lifecycle scripts may require source files during installation. Copy those
files before the install, or explicitly defer scripts if the project supports it.

For a workspace package, [deploy](/cli/deploy) can prepare a target directory
with publishable files and installed dependencies:

```sh
aube --filter @acme/api deploy dist/api
```

## When CI rejects the lockfile

Reproduce the failure locally with `aube install --frozen-lockfile`. If the
manifest change was intentional, run `aube install`, review the resulting diff,
and commit the updated lockfile. See [troubleshooting](/troubleshooting#the-lockfile-is-out-of-sync)
for repair options.
