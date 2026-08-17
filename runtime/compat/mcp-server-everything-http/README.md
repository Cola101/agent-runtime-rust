# Official MCP Streamable HTTP compatibility fixture

This directory contains only a minimal npm manifest and its reviewed lockfile.
It must not contain `node_modules` or an npm cache.

The release gate installs the exact dependency graph into a temporary directory,
starts `@modelcontextprotocol/server-everything@2026.7.4` with a cleared
environment, runs the external protocol and full Agent Loop tests, stops the
exact child PID, and removes the complete temporary directory.

Run from the repository root:

```text
runtime/scripts/test-mcp-streamable-http-compat.sh
```

Changing the manifest, lockfile, server version, SDK version, or lock digest
requires a new external compatibility review and evidence update.
