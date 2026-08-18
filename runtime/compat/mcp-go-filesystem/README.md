# mark3labs/mcp-go compatibility fixture

This release gate consumes, but does not vendor, the independently maintained
[`mark3labs/mcp-filesystem-server`](https://github.com/mark3labs/mcp-filesystem-server).
It is used to prove interoperability with a non-official Go MCP protocol stack.

Pinned inputs:

- Server tag: `v0.11.1`
- Server commit: `5646396f50ba144b9dd1ca9d088db0ac08cab3f8`
- Git tree: `8dcf90035679d3f7a9ed509f941efdd36d9abe85`
- `go.mod` SHA-256: `f967edd0f15e9cfa53bf7cd2eb5b3fd5290463c65faf3bbdf6bfc944d0453c7c`
- `go.sum` SHA-256: `f869f93873eb5bc27309b948581d6c205cca7ca7b7a7f69909c598106676dbe1`
- Protocol implementation: `github.com/mark3labs/mcp-go v0.32.0`
- License: MIT

Run from the repository root:

```sh
runtime/scripts/test-mcp-go-filesystem-compat.sh
```

The script clones and builds only inside a guarded temporary directory with an
isolated HOME, Go module cache and build cache. The Agent allowlist contains
only `list_allowed_directories`; no file read, write, move or deletion Tool is
available to the model. The test uses explicit MCP `2025-03-26` authority and
requires exactly one ignored compatibility test to pass.
