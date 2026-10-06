# fsql for Zed

Language support for `.fsql` files: syntax highlighting, indentation, matching
parentheses, quote completion, and `--` comment toggling. Uses the bundled fsql
grammar, pinned to a merged commit, plus a small [TypeScript LSP] for completion
and hover. A Rust adapter launches the server.

[TypeScript LSP]: ../../packages/fsql-lsp/README.md

## Install locally

Install Bun 1.4.2+, Node 22.18+, and Rust through rustup. From the repository root:

```sh
runner install
run lsp:build
run --dir packages/fsql-lsp bun link
```

Keep Bun's binary directory and Node on your shell's PATH so Zed can run `fsql-lsp`.
Alternatively, configure an explicit command in Zed settings:

```json
{
  "lsp": {
    "fsql": {
      "binary": {
        "path": "/absolute/path/to/node", // or: "node"
        "arguments": ["/absolute/path/to/fsql/packages/fsql-lsp/dist/node/server.mjs", "--stdio"]
      }
    }
  }
}
```

In Zed, run **zed: install dev extension** from the command palette and select
this directory (`editors/zed`, containing `extension.toml`). Then open
`tree-sitter-fsql/examples/browse.fsql` from the repository root. Zed builds the
grammar and adapter on installation; use **Rebuild** in the Extensions page after changes.
For installation errors, run **zed: open log**.

## Development

From the repository root, run `runner install --frozen`, then
`run -s grammar:check lsp:check lsp:test`.
That check compiles all of this extension's queries against the fsql grammar.
The highlights are adapted from `tree-sitter-fsql/queries/highlights.scm`, with
Zed capture names; indentation uses Zed's `@indent`, `@start`, and `@end` captures.
When updating the grammar, update the manifest's pinned revision and rebuild.

The extension is not yet published in the Zed registry. A registry entry would
use this repository with `path = "editors/zed"`.

<!-- rumdl-disable-file line-length -->
