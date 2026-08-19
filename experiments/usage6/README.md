# usage 6.x adoption experiment

This generated crate proves that usage's static tables can express aube's current
checked-in spec: 118 commands, 623 flags, and 89 positional arguments are emitted
without dropping a spec field. It intentionally pins usage to a git revision and
is not intended to merge before usage 6.x.

It is not yet the typed aube CLI. Moving the real clap structs exposes work that a
String shadow cannot test:

- fixed arity and distinct value names from `num_args`;
- relationship families such as `conflicts_with_all`;
- custom help/version actions and disabled built-in flags;
- `after_long_help`, value hints, and clap help-formatting policy;
- clap value enums and custom value parsers on aube's domain types;
- rename policy and command metadata that are not literal usage attributes.

The generated source is kept in this PR as a concrete compile target and baseline
for converting the real types. Regenerate it from the usage repository with:

```console
cargo run -p xtask -- gen-shadow /path/to/aube/aube.usage.kdl /path/to/aube/experiments/usage6 usage
```

