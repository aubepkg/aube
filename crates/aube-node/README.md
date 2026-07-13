# aube Node-API proof of concept

This unpublished crate tests whether a host such as OpenCode can call aube's
command layer through a Node-API addon embedded in a compiled Bun executable.
It is intentionally limited to one async operation:

```ts
await install(projectDirectory)
```

The package manifest must already declare its dependencies. The addon always
skips root and dependency lifecycle scripts.

Run the direct Bun and compiled-executable smoke tests from the repository
root:

```sh
crates/aube-node/poc/run.sh
```

The script uses Bun 1.3.11 through mise, matching the Bun version OpenCode
currently declares. It builds the addon with the `napi` Cargo profile, runs it
directly, embeds it with `bun build --compile`, and runs the resulting
standalone executable.

This proof of concept does not define the eventual package-add, progress,
cancellation, or cross-platform distribution APIs.
