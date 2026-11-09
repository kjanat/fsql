# fsql LSP

fsql-lsp is a small TypeScript language server for fsql. The stdio server runs directly under Bun, Node 22.18+, or Deno, and browser integration uses a compiled ESM worker. The server provides built-in table, column, function, and keyword completion and hover. Completion recognizes table positions and explicit qualifiers such as `files.`, replaces the current word, and ignores comments and quoted text.

The server uses a static catalog and resolves no aliases, CTEs or query scopes. It provides no diagnostics or formatting and executes no queries. The Rust engine decides SQL validity and runtime behavior.

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

These commands run the TypeScript source and need no build step or global link. The Deno command reads the installed `node_modules` and writes no lockfile.

For editor installation, see [the Zed extension]. Other LSP clients can launch the same runtime command with an absolute path to `packages/fsql-lsp/src/server.ts`. The server's stdout carries LSP messages only.

## Browser integration

`bun run --cwd packages/fsql-lsp build` produces two entry points, with source maps:

- `dist/node/server.mjs` is the stdio server. It leaves runtime dependencies external.
- `dist/browser/worker.mjs` is a self-contained module worker that bundles the LSP dependencies.

Serve `dist/browser/worker.mjs` as JavaScript and connect your web editor's LSP client to the worker's `postMessage` transport:

```js
const worker = new Worker('/fsql-lsp/worker.mjs', { type: 'module' });
```

The worker exchanges JSON-RPC message objects, without stdio `Content-Length` framing. Use a browser LSP client's message reader and writer to initialize it, sync documents, and request completion or hover. Bundler integrations can resolve the worker through the `@kjanat/fsql-lsp/worker` export. The host application supplies the editor and the LSP client.

## Development

From the repository root:

```sh
bun run --cwd packages/fsql-lsp check
bun run --cwd packages/fsql-lsp build
bun test --cwd packages/fsql-lsp
```

Tests exercise the source server under Bun and Node, the compiled Node server, and the compiled worker's protocol under Bun's Worker runtime. When Deno is installed, they also exercise the source server under Deno.

[the Zed extension]: ../../editors/zed/README.md
