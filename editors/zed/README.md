# fsql for Zed

Language support for `.fsql` files: syntax highlighting, indentation, matching parentheses, quote completion, and `--` comment toggling. Uses the bundled fsql grammar, pinned to a merged commit, plus a small [TypeScript LSP] for completion and hover. A Rust adapter launches the server.

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

In Zed, run **zed: install dev extension** from the command palette and select this directory (`editors/zed`, containing `extension.toml`). Then open `tree-sitter-fsql/examples/browse.fsql` from the repository root. Zed builds the grammar and adapter on installation; use **Rebuild** in the Extensions page after changes. For installation errors, run **zed: open log**.

Use an absolute path to Bun if it is not on Zed's PATH. Node 22.18+ can use the same arguments with `"path": "node"`; see the [TypeScript LSP] instructions for Deno. No LSP build or global linking is required.

## Development

From the repository root:

```sh
bun install --frozen-lockfile
bun run --cwd tree-sitter-fsql check
bun run --cwd packages/fsql-lsp check
bun run --cwd packages/fsql-lsp build
bun test --cwd packages/fsql-lsp
```

The grammar check compiles all of this extension's queries. The highlights are adapted from [`tree-sitter-fsql/queries/highlights.scm`], with Zed capture names; indentation uses Zed's `@indent`, `@start`, and `@end` captures. When updating the grammar, update the manifest's pinned revision and rebuild.

The extension is not (yet?) published in the Zed registry. A registry entry would use this repository with `path = "editors/zed"`.

[`tree-sitter-fsql/queries/highlights.scm`]: https://github.com/kjanat/fsql/blob/5b4cb65c7b51b5a8432e6304339a0eda49ec9c13/tree-sitter-fsql/queries/highlights.scm
