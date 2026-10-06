# Database-backed editor call hierarchy

`trace-lsp` is a separate executable that reads an existing trace SQLite
snapshot. Generate the database before starting the server:

```sh
cargo build -p trace-cli -p trace-lsp --release
target/release/trace analyze ./project -o analysis.db
target/release/trace-lsp --db analysis.db
```

The `--db` option is required. Both minimal and full exports of schema v7 work,
without `--debug-points-to`. Missing, invalid, older, and newer databases are
rejected on startup with errors on stderr. The server never creates databases,
migrates schemas, or adds indexes. Stdout contains only Content-Length-framed
JSON-RPC messages; logs and startup errors go to stderr.

## Connecting an editor

For Neovim with its built-in LSP client, run this Lua from your project root,
with `trace-lsp` on PATH and `analysis.db` in the project root:

```lua
vim.lsp.start({
  name = 'trace-lsp',
  cmd = { 'trace-lsp', '--db', vim.fn.fnamemodify('analysis.db', ':p') },
  root_dir = vim.fn.getcwd(),
})
-- With the cursor inside a function:
vim.lsp.buf.incoming_calls()
vim.lsp.buf.outgoing_calls()
```

See the [Neovim LSP client documentation](https://neovim.io/doc/user/lsp/).
Other clients can launch the same command as an stdio language server. The
server advertises only `callHierarchyProvider` and UTF-16 position encoding.
It implements `initialize`/`initialized`, `shutdown`/`exit`, and the three
[standard call hierarchy requests](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_prepareCallHierarchy).
Unsupported requests return MethodNotFound; malformed parameters and stale
items return InvalidParams. Exit after shutdown succeeds; exit without
shutdown fails. Notifications about source changes are ignored.

## Identity and graph behavior

Prepare call hierarchy at any line inside a function's inclusive exported
line range. All matching functions are returned, even if they occupy the same
line or represent different link images. Because precise function columns
are not stored, the character position does not disambiguate these candidates.
An unknown file or position returns null. Preparing targets at individual call
sites is outside the initial implementation.

Items display the stored signature, linkage, definition/declaration status,
dependency status, and link target name/output/ID when present. Their opaque
`data` preserves the database function ID and server session. Subsequent
requests use that identity, keeping overloads, file-static functions, and
target-distinct definitions separate. Reuse items only within the session
that issued them; unknown, malformed, and other-session identities are rejected.

Each incoming/outgoing request expands one graph level, including all stored
direct, indirect, and ambiguous targets. Repeated calls are grouped by function
ID, with distinct source ranges sorted by position. Peers are sorted by ID.
Recursive edges and cycles remain available for further client navigation.
No callees are invented for unresolved call sites.

Synthetic IPC edges remain navigable between source-located functions with
empty `fromRanges`. Clients may show the relationship without a selectable
call site; their rendering depends on the editor. Synthesized external
functions with `line_start = 0` cannot supply a source URI and are omitted.
Real prototypes and declarations from dependency headers remain navigable.

## Paths and source ranges

File URIs are decoded and matched to exact normalized database paths, never
by basename or substring. Spaces, Unicode, and URI escaping are supported.
Existing local symlinks are resolved, including existing parents of missing
files. Only local `file` URIs without queries or fragments are accepted.
Relative stored paths require one unambiguous recorded `target_root` across
all analysis runs; a relative target root is interpreted relative to the
database directory. Relative paths in a merged database or a database with
different source roots are rejected with a startup error, because the schema
does not associate files with individual runs. `trace-merge` records its input
database paths in `target_root`, not a source root. Absolute stored paths work
with either kind of metadata and do not depend on `target_root`.

For databases generated in another checkout on the same operating system,
pass repeatable `--path-map DB_PREFIX=LOCAL_PREFIX` arguments. Both prefixes
must be absolute paths supplied at runtime. The longest matching component
prefix wins; the last supplied mapping wins ties. Mapping changes returned
URIs and source reads, while database lookups retain the original exact path.
There is no automatic basename fallback or cross-platform drive/separator
translation. For example, with environment variables naming the two roots:

```sh
trace-lsp --db analysis.db --path-map "$RECORDED_ROOT=$LOCAL_ROOT"
```

The database format supplies function line ranges and call points, without
precise function-name or complete call-expression spans. The server uses this
consistent policy without adding persisted columns:

- Function `range` starts at character zero of the first exported line and
  ends at the UTF-16 length of the last exported line. `selectionRange` is a
  point at the start, always contained in `range`; no function-name span is guessed.
- Call `fromRanges` are point ranges at the stored call positions. One-based
  Unicode scalar columns from the preprocessor are converted to zero-based
  UTF-16 positions using source text, with columns clamped to the line length.
- Macro call ranges follow the consumer presentation policy defined in
  [Call source locations](ANALYSIS.md#call-source-locations).
- If source text is missing or cannot be decoded as UTF-8, a function range
  ends at character zero of the line following its last exported line. Call
  positions retain their recorded line and fall back to character zero.
  Available files with missing lines use the same fallbacks. Source URIs are
  still returned, so the client can report the unavailable file.

## Snapshot and performance

The database is opened read-only and pinned with one read transaction for
the server lifetime. Replacing or rebuilding it requires restarting the
server. An in-place writer may be blocked by this read transaction. Startup
reads wait up to five seconds for a conflicting database lock before reporting
an error. Source files are read lazily and cached on first access, including
missing files.
Source edits never update analysis; changed source text may make snapshot
locations stale. Restart after regenerating the database and keep source
files at the analyzed revision for accurate positions.

There are no live diagnostics, re-analysis, completion, rename, references,
hover, or symbol-search capabilities. Request framing is bounded to 8 MiB
messages and 8 KiB headers; malformed framing terminates with a stderr error,
while invalid JSON receives a ParseError response and processing continues.

The analyzer/export pipeline and schema are unchanged. New queries are opt-in
helpers in `trace-db`'s shared inspect layer; the original CLI does not execute
them. The LSP URI dependency belongs only to `trace-lsp`. The server builds an
exact file-path map once and uses existing call-edge indexes to expand one
level, fetching each peer once and caching source text. It does not load or
traverse the entire call graph per request.

The [LSP isolation measurements](EVAL_REPORT.md#database-backed-lsp-isolation-200)
compare release analyzer builds and verify identical exported analysis data.
