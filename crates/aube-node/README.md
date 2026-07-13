# aube Node-API proof of concept

This unpublished crate tests whether a host such as OpenCode can call aube's
command layer through a Node-API addon embedded in a compiled Bun executable.
It exposes the async operation OpenCode's npm service needs:

```ts
await install(projectDirectory, {
  add: [{ name: "@opencode-ai/plugin", version: opencodeVersion }],
})
```

The addon creates an empty package manifest when needed, saves added packages
as exact production dependencies, and installs declared dependencies. It always
skips root and dependency lifecycle scripts. Calls are serialized inside the
addon while aube's remaining command-scoped global state is made reentrant.

Run the direct Bun and compiled-executable smoke tests from the repository
root:

```sh
crates/aube-node/poc/run.sh
```

The script uses Bun 1.3.11 through mise, matching the Bun version OpenCode
currently declares. It builds the addon with the `napi` Cargo profile, runs it
directly, embeds it with `bun build --compile`, and runs the resulting
standalone executable.

The remaining production work is progress and cancellation integration,
reentrant parallel installs, structured error objects beyond the stable code
prefix in each rejected promise, and cross-platform artifact distribution.
