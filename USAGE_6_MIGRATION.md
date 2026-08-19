# usage 6.x migration status

This branch converts aube's real CLI structs and attributes from clap to
usage-rs and removes clap from the runtime dependency graph. It is intentionally
kept as an experimental PR until usage 6.x exists.

The conversion currently does not compile. `cargo check -p aube` reaches the
usage derives and reports 194 errors. Most are consequences of these distinct
gaps:

- usage subcommand variants must be bare or wrap one Args struct; aube has
  inline struct variants throughout nested command enums;
- one Args type cannot currently be reused by multiple commands, while aube
  flattens shared network, lockfile, and virtual-store groups broadly;
- aube's custom `CommandFactory`, `FromArgMatches`, and `ArgMatches` paths have
  no usage-rs replacement API;
- flag aliases, `rename_all`, fixed `num_args`, expression-valued defaults,
  custom value parsers, and several help/action policies are not accepted by
  the usage derives;
- usage's `ValueEnum` supplies `FromStr`, which conflicts with aube enums that
  already derive or implement `FromStr` for non-CLI use;
- positional placeholder spelling differs (`name` versus clap's `value_name`),
  so a mechanical attribute rewrite is not lossless.

The remaining compiler errors are deliberately left visible: restoring clap or
adding a parallel shadow would hide the exact work required in usage before the
real CLI can move.
