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

On Linux and Apple Silicon macOS, `mise run build` and `mise run test`
provision the pinned Blaze and mbx prebuilts and their matching Rust compiler
automatically. The shared setup fails if a verified archive is unavailable.
Windows uses the plain Cargo tasks and does not require Blaze.

mbx shares dependency compilation between checkouts and manages inactive
`target/` directories. Linux build and test CI jobs use its object cache.
Release and custom-profile builds continue through Cargo and mbx.

For another Cargo command through the pinned Blaze setup, use:

```sh
sh scripts/setup-blaze.sh cargo check
```

For a final check with the normal Rust compiler while retaining mbx caching:

On macOS and Linux:

```sh
MBX_BLAZE=0 mise exec -- cargo test
```

On Windows PowerShell:

```powershell
$env:MBX_BLAZE = '0'
mise exec -- cargo test
```

See [the contributor guide](docs/contributing.md) for the pinned toolchain,
CI setup, supported platforms, and validation limits.
