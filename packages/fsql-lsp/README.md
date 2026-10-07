# fsql LSP

A small TypeScript language server for fsql. Run the stdio server directly with Bun, Node 22.18+, or Deno; browser integration uses a compiled ESM worker. Provides built-in table, column, function, and keyword completion and hover. Completion recognizes table positions and explicit qualifiers such as `files.`; it replaces the current word and ignores comments and quoted text.

This initial server uses a static catalog. It does not resolve aliases, CTEs or query scopes, provide diagnostics, format queries, or execute fsql. SQL validity and runtime behavior remain the Rust engine's responsibility.

Install dependencies from the repository root using Bun 1.4.2+:

```sh
bun install --frozen-lockfile
```

Then launch the TypeScript source with any one of these commands:

```sh
bun packages/fsql-lsp/src/server.ts --stdio
node packages/fsql-lsp/src/server.ts --stdio
deno run --allow-env --allow-read --node-modules-dir=manual --no-lock packages/fsql-lsp/src/server.ts --stdio
```

No build or global linking is required. The Deno command uses the installed `node_modules` and does not create a separate lockfile.

For editor installation, see [the Zed extension]. Other LSP clients can launch the same runtime command with an absolute path to `packages/fsql-lsp/src/server.ts`. The server writes only LSP messages to stdout.

## Browser integration

`bun run --cwd packages/fsql-lsp build` produces two entry points, with source maps:

- `dist/node/server.mjs`: stdio server; runtime dependencies remain external.
- `dist/browser/worker.mjs`: self-contained module worker, including LSP dependencies.

Serve `dist/browser/worker.mjs` as JavaScript and connect your web editor's LSP client to the worker's `postMessage` transport:

```js
const worker = new Worker('/fsql-lsp/worker.mjs', { type: 'module' });
```

The worker exchanges JSON-RPC message objects, without stdio `Content-Length` framing. Use a browser LSP client's message reader/writer to initialize it, sync documents, and request completion or hover. Bundler integrations can resolve the worker through the `@kjanat/fsql-lsp/worker` export. This package supplies the server; the host application supplies its editor and LSP client.

## Development

From the repository root:

```sh
bun run --cwd packages/fsql-lsp check
bun run --cwd packages/fsql-lsp build
bun test --cwd packages/fsql-lsp
```

Tests exercise the source server under Bun and Node, the compiled Node server, and the compiled worker's protocol using Bun's Worker runtime. When Deno is installed, they also exercise the source server under Deno.

[the Zed extension]: ../../editors/zed/README.md
