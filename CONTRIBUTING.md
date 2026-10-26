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

`mise run build` and `mise run test` provision the pinned Blaze and mbx
prebuilts and their matching Rust compiler automatically. The shared setup
fails if a verified archive is unavailable. Linux GNU x64/ARM64 prebuilts are
available; Apple Silicon macOS remains a merge blocker until its archive
is published. Windows CI retains its existing Rust build path.

mbx shares dependency compilation between checkouts and manages inactive
`target/` directories. The build and Linux test CI jobs use its object cache.
Release and custom-profile builds continue through Cargo and mbx.

For a final check with the normal Rust compiler while retaining mbx caching:

```sh
mise exec -- env MBX_BLAZE=0 cargo test
```

See [the contributor guide](docs/contributing.md) for the pinned toolchain,
CI setup, supported platforms, and validation limits.
