//! Integration test for an embedder whose public invocation is a host
//! subcommand rather than a standalone binary.

use aube_util::{AUBE, Embedder, cmd, command_prefix_display, prog, recursive_command_args};

static ELIDELIKE: Embedder = Embedder {
    command_prefix: &["elide", "aube", "--"],
    ..AUBE
};

#[test]
fn embedded_command_prefix_drives_user_facing_commands_and_recursion() {
    aube_util::set_embedder(&ELIDELIKE);

    assert_eq!(prog(), "aube");
    assert_eq!(command_prefix_display(), "elide aube --");
    assert_eq!(recursive_command_args(), &["aube", "--"]);
    assert_eq!(cmd("install"), "elide aube -- install");
}
