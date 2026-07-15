# aube Node-API bindings

`@jdxcode/aube-node` lets a host such as OpenCode call aube's command layer
through a Node-API addon, including from a compiled Bun executable. It exposes
the async operation OpenCode's npm service needs:

```ts
await install(projectDirectory, {
  add: [{ name: "@opencode-ai/plugin", version: opencodeVersion }],
  signal: abortController.signal,
  onEvent(event) {
    // phase, progress, and output events
  },
})
```

The addon creates an empty package manifest when needed, saves added packages
as exact production dependencies, and installs declared dependencies. It always
skips root and dependency lifecycle scripts. Independent projects install in
parallel using invocation-scoped runtime, script, dependency-chain, directory,
and project-lock state. Registry/auth settings and dependency-section omission
come from the project's `.npmrc`; `omit=dev` and `omit=optional` are honored.

`onEvent` is delivered through a non-blocking Node-API thread-safe function.
`signal` cooperatively cancels the invocation at a safe install boundary.
Rejected promises are JavaScript `Error` objects with stable `code` and
human-readable `diagnostic` properties.

Run the direct Bun and compiled-executable smoke tests from the repository
root:

```sh
crates/aube-node/poc/run.sh
```

The script uses Bun 1.3.11 through mise, matching the Bun version OpenCode
currently declares. It builds the addon with the `napi` Cargo profile, runs it
directly, embeds it with `bun build --compile`, and runs the resulting
standalone executable. Its local registry uses a two-request barrier so the
smoke test fails if independent addon calls become serialized. It also covers
structured events, in-flight and pre-requested cancellation, local `file:`
dependencies, `.npmrc` dependency omission, and structured rejection
properties.

CI packages the addon as `@jdxcode/aube-node` plus platform packages for the
eight macOS, Windows, glibc Linux, and musl Linux environments OpenCode ships.
Pull requests and `main` builds expose npm tarballs as workflow artifacts for
smoke testing; tagged releases publish the same tarballs through npm trusted
publishing. OpenCode's x64 baseline builds reuse the corresponding x64 addon.

Compiled Bun hosts should add `bunPlugin({ os, arch, libc })` from
`@jdxcode/aube-node/bun-plugin` to each target's `Bun.build` plugins. It
resolves the root import to that target's native package, embedding one addon
per binary. Bundling the root loader directly would conservatively include
every static platform branch.

Cross-compilation hosts must install all optional platform packages before
building targets for other operating systems and architectures. OpenCode's
build preparation should use its pinned addon version:

```sh
bun install --os="*" --cpu="*" @jdxcode/aube-node@$AUBE_NODE_VERSION
```
