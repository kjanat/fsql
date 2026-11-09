# fsql for Zed

The extension gives Zed syntax highlighting, indentation, matching parentheses, quote completion, and `--` comment toggling for `.fsql` files. It uses this repository's fsql grammar at a pinned, merged commit and a small [TypeScript LSP] for completion and hover. A Rust adapter launches the server.

[TypeScript LSP]: ../../packages/fsql-lsp/README.md

## Install locally

Install Bun 1.4.2+ and Rust through rustup. From the repository root:

```sh
bun install --frozen-lockfile
```

Configure Zed to launch the TypeScript server directly:

```json
{
  "lsp": {
    "fsql": {
      "binary": {
        "path": "bun",
        "arguments": ["/absolute/path/to/fsql/packages/fsql-lsp/src/server.ts", "--stdio"]
      }
    }
  }
}
```

In Zed, run **zed: install dev extension** from the command palette and select this directory, `editors/zed`, which contains `extension.toml`. Then open `tree-sitter-fsql/examples/browse.fsql` from the repository root. Zed builds the grammar and adapter on installation; use **Rebuild** in the Extensions page after changes. For installation errors, run **zed: open log**.

Use an absolute path to Bun if it is not on Zed's PATH. Node 22.18+ can use the same arguments with `"path": "node"`; see the [TypeScript LSP] instructions for Deno. The LSP needs no build step or global link.

## Development

From the repository root:

```sh
bun install --frozen-lockfile
bun run --cwd tree-sitter-fsql check
bun run --cwd packages/fsql-lsp check
bun run --cwd packages/fsql-lsp build
bun test --cwd packages/fsql-lsp
```

The grammar check compiles all of this extension's queries. The highlight queries adapt [`tree-sitter-fsql/queries/highlights.scm`] with Zed capture names, and the indentation queries use Zed's `@indent`, `@start`, and `@end` captures. After updating the grammar, update the manifest's pinned revision and rebuild.

The Zed extension registry has no entry for this extension. A registry entry would use this repository with `path = "editors/zed"`.

[`tree-sitter-fsql/queries/highlights.scm`]: https://github.com/kjanat/fsql/blob/5b4cb65c7b51b5a8432e6304339a0eda49ec9c13/tree-sitter-fsql/queries/highlights.scm
