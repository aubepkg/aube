# aube Node-API proof of concept

This unpublished crate tests whether a host such as OpenCode can call aube's
command layer through a Node-API addon embedded in a compiled Bun executable.
It exposes the async operation OpenCode's npm service needs:

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
and project-lock state.

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
structured events, cancellation, and structured rejection properties.

The remaining production work is cross-platform artifact distribution and the
OpenCode adapter that replaces its Arborist install call.

CI packages the addon as `@jdxcode/aube-node` plus platform packages for the
eight macOS, Windows, glibc Linux, and musl Linux environments OpenCode ships.
Pull requests and `main` builds expose npm tarballs as workflow artifacts for
smoke testing; tagged releases publish the same tarballs through npm trusted
publishing. OpenCode's x64 baseline builds reuse the corresponding x64 addon.
