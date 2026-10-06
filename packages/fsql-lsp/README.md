# fsql LSP

A small TypeScript language server for fsql, built as ESM for Node stdio and
browser workers. Use Bun 1.4.2+ and Node 22.18+ for development.
Provides built-in table, column, function, and keyword completion and hover.
Completion recognizes table positions and explicit qualifiers such as `files.`;
it replaces the current word and ignores comments and quoted text.

This initial server uses a static catalog. It does not resolve aliases, CTEs or
query scopes, provide diagnostics, format queries, or execute fsql. SQL validity
and runtime behavior remain the Rust engine's responsibility.

From the repository root:

```sh
runner install
run lsp:build
run lsp:check
run lsp:test
run lsp:start
```

For editor installation, see [the Zed extension](../../editors/zed/README.md).
Other LSP clients can launch `node /absolute/path/to/packages/fsql-lsp/dist/node/server.mjs --stdio`.
Launch the server directly in clients so Runner's task output stays out of the
protocol stream. The server writes only LSP messages to stdout.

## Browser integration

`run lsp:build` produces two entry points, with source maps:

- `dist/node/server.mjs`: stdio server; runtime dependencies remain external.
- `dist/browser/worker.mjs`: self-contained module worker, including LSP dependencies.

Serve `dist/browser/worker.mjs` as JavaScript and connect your web editor's LSP
client to the worker's `postMessage` transport:

```js
const worker = new Worker('/fsql-lsp/worker.mjs', { type: 'module' });
```

The worker exchanges JSON-RPC message objects, without stdio `Content-Length`
framing. Use a browser LSP client's message reader/writer to initialize it, sync
documents, and request completion or hover. Bundler integrations can resolve the
worker through the `@kjanat/fsql-lsp/worker` export. This package supplies the
server; the host application supplies its editor and LSP client.

`run lsp:test` builds first, then tests the source server, compiled Node server,
and compiled worker's protocol using Bun's Worker runtime.

<!-- rumdl-disable-file line-length -->
