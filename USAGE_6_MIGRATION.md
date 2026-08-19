# usage 6.x migration status

This branch converts aube's real typed CLI from clap to usage-rs and removes
clap from the runtime dependency graph. The conversion compiles, its 806 library
tests pass, and clippy passes for the aube crate. It remains an experimental PR
until usage 6.x is published because its manifest deliberately pins a stacked
git revision.

The working port still exposes release gaps that are tracked in jdx/usage's
6.x plan:

- relationships that cross a flattened Args boundary are enforced by small
  post-bind checks until usage validates the composed command;
- flattened clap help headings are not represented, so the port preserves
  parsing but cannot yet reproduce every long-help section;
- dynamic embedder names can be applied to emitted KDL, but parser help and
  diagnostics retain the static `aube` identity;
- command effects and completion generation parse the derived KDL with
  usage-lib, raising the dependency MSRV above the argv-only tier;
- the root remains permissive intentionally because aube's external-subcommand
  path forwards package-manager commands and options.
