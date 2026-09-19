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

mise wraps `cargo` with [mbx](https://mr-boxington.jdx.dev), so compiled work
is shared across checkouts. `mise run` tasks and `mise exec -- cargo …` always
use the wrapper; plain `cargo` does too once mise is activated in your shell
(`mise activate`).
