# Contributing

Read the [contributing guide](docs/contributing.md) for setup, tests,
documentation, and pull-request expectations. The same guide is available
[on the docs site](https://aube.sh/contributing).

```sh
mise install
mise run build
mise run test
mise run docs:dev
```

## mbx build cache

mise wraps `cargo` with [mbx](https://mr-boxington.jdx.dev), so compiled work is
shared across checkouts. `mise run` tasks and `mise exec -- cargo …` use the
wrapper; plain `cargo` does too once mise is [activated in your
shell](https://mise.jdx.dev/getting-started.html#activate-mise). Builds that set
`MBX_DISABLE=1`, including CI, skip the cache.

mbx also shares build results between worktrees and manages each checkout's
`target/` directory, collecting inactive targets as the configured age and
size limits are reached.

On Linux and macOS Apple silicon, install [Blaze](https://github.com/indexable-inc/blaze)
to accelerate repeated development builds. mise enables mbx's Blaze routing by
default; once Blaze's `ra-jit` executable is on `PATH`, ordinary dev
`cargo build`, `cargo check`, `cargo test`, and `cargo run` commands use it.
The first build loads the workspace, and later edits can rebuild quickly.
Release and custom-profile builds continue through Cargo and mbx. Blaze is
experimental and uses extra memory, so use the normal Cargo or CI build for
final validation.
