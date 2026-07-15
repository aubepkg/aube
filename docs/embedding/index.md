# Embedding aube

aube can run inside another application instead of being invoked as a child
process. Embedding avoids CLI startup and output parsing, gives the host
structured progress events and stable error codes, and allows cooperative
cancellation.

The native Rust API is the foundation for all embedding integrations. Runtime
adapters, such as Node-API bindings, should be thin translations over this API.

## Supported operations

The stable `aube::embed` facade supports:

- installing a project's declared dependencies;
- adding packages and installing the resulting dependency graph;
- per-install structured progress and output events;
- cooperative cancellation;
- concurrent operations in unrelated projects; and
- stable `ERR_AUBE_*` diagnostic codes.

Operations that target the same workspace are serialized. The project lock
spans manifest changes, lockfile updates, and linking, so concurrent callers do
not observe partial state.

See [Embedding in Rust](./rust.md) for the native API. JavaScript adapters
should translate this facade through Node-API rather than calling Rust through
a C FFI layer.

## Compatibility

The `aube::embed` module is the supported in-process interface. The
`aube::commands` modules remain public for CLI wrappers, but their command
argument types follow the CLI and are not the preferred API for native hosts.

Every error and warning emitted by aube carries a stable identifier. See the
[error code reference](../error-codes.md) for the compatibility policy.

For questions and integration discussion, use
[GitHub Discussions](https://github.com/jdx/aube/discussions).
