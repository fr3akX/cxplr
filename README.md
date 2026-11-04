# code-explore

Learn an unfamiliar repository top-down. Point it at a project and get a local web
map of its components, how they interact and the main data flows. Then drill into any
component for the next level of detail, down to the cited source lines.

| Level | Shows |
| --- | --- |
| System | Main components, datastores, external systems, actors; 2–6 end-to-end data flows |
| Component (repeatable) | Internal modules/classes/functions, how they handle the component's interfaces |
| Source | The exact file and line behind every claim, plus directory browsing |

The exploration is done by [Codex](https://github.com/openai/codex) (`codex exec`)
running in a read-only sandbox inside the target repository. code-explore builds the
prompt for each zoom level, constrains the answer to a JSON schema, checks every
citation against the files on disk, caches the result and serves the UI.

## Requirements

- Rust toolchain (to build)
- `codex` on `PATH`, already logged in; the model and reasoning effort come from your
  Codex config unless `--model` is given

## Run

```sh
cargo build --release
./target/release/code-explore /path/to/repo --index   # map the project, then open the browser
./target/release/code-explore /path/to/repo           # browse what is cached: http://127.0.0.1:7878
```

`--index` explores the system map and then every component worth a deeper map,
several at a time, prints progress, and opens the browser when done. Browsing then
reads everything from the cache and never calls Codex on its own: a level that is
not indexed shows **Explore now**, and stale maps offer **Refresh**.

| Indexing option | Default | Meaning |
| --- | --- | --- |
| `--depth N` | 2 | Levels to map: 1 = system, 2 = system and its components, … |
| `--jobs N` | 4 | Explorations running at the same time |
| `--min-lines N` | 300 | Components with fewer text lines stay leaves (the source view covers them) |
| `--index-only` | | Index and exit (exit code 1 if any exploration failed) |

Indexing is resumable and incremental: fresh maps are skipped, so re-running after an
interruption or a commit only explores what is missing or changed. For reference,
`sre-agent` (≈850 files) indexed at depth 2 in about 16 minutes with 9 explorations
and 3.5M input tokens.

Other options: `--port`, `--host` (keep loopback; the API serves repository files),
`--open`, `--model`, `--codex <path>`, `--timeout-mins` (per exploration or question,
default 20), `--cache-dir`, `--lsp LANG=COMMAND` (repeatable), `--no-lsp`.

## Using the map

- Click a box or arrow: summary, paths, interactions and evidence in the side panel.
- Double-click a box, or **Explore inside**: map that component one level deeper.
  Breadcrumbs at the top go back up.
- Click a data flow: its steps are highlighted and numbered on the map.
- Click a citation or path: source viewer at that line.
- Drag a box to move it when labels overlap; edges follow. Positions are saved per
  map (repository, commit, level) in browser storage. **Reset layout** discards them.
- Mouse wheel zooms, dragging the background pans, `F` fits, `Esc` clears the selection.
- Box labels show whether the inner map is cached: **open ›** (fresh), **open
  (stale) ›**, or **explore ›** (not explored yet).

## Simulation

Each data flow has **▶**: N inputs (set in **Inputs**, 1–20) travel the flow's steps
as tokens, staggered, pausing at each box. When a step reaches a component with its
own cached map, **Follow inside** opens that map and plays the flow there that
involves the component the input came from.

**Example inputs** under a selected flow asks Codex (read-only, about 1–3 minutes,
cached) for 2–5 concrete inputs that take different branches: the normal path,
rejections, failures, retries. Each one has its own lettered token and route, shows
its data at every hop while it plays, and cites the code that decides each step.

## Reading source

The source viewer highlights syntax (highlight.js, bundled; no network access).

- Click a name: the side pane shows its documentation/signature, definitions and, on
  request, references. Ctrl/⌘-click jumps straight to a single definition.
- **‹ / ›** or Alt+←/→ walk the navigation history.
- The chip in the header shows the language server state. Files without a server
  (YAML, shell, configs, …) or with a failed server fall back to whole-word text
  search across the repository (respecting `.gitignore`).

Language servers are found on `PATH` and started on first use, one per language,
with the repository as workspace:

| Language | Servers tried |
| --- | --- |
| Rust | `rust-analyzer` (build scripts and proc macros disabled) |
| Python | `pyright-langserver --stdio`, `basedpyright-langserver --stdio`, `pylsp`, `jedi-language-server` |
| TypeScript / JavaScript | `typescript-language-server --stdio` |
| Go | `gopls` |
| C / C++ | `clangd` |

Override or add one with `--lsp`, e.g. pyright without a global install:

```sh
code-explore /path/to/repo --lsp "python=npx -y -p pyright@1 pyright-langserver --stdio"
```

`--lsp python=` disables a language. Language servers can run project tooling
(for example `gopls` runs `go list`, TypeScript loads tsconfig plugins); use
`--no-lsp` for repositories you do not trust.

## Asking questions

The **Ask** tab sends a question to Codex (same read-only sandbox) together with what
you are looking at: the current map, the selected component, and code lines chosen
with **Ask about selection** in the source viewer (whole file if nothing is
selected). Earlier questions in the conversation are included for follow-ups.

Answers are Markdown; every `path:line` in them opens the source viewer, and the
citation list is checked against the files like map evidence. The conversation lives
in the page and is lost on reload.

## Trust model

- Maps are model-generated. ✓ on a citation means the file exists inside the
  repository and has that line; it does not prove the claim. `!` marks citations
  that failed the check, and dropped references are listed under Diagnostics.
- Codex runs with `--sandbox read-only`, approvals off, and hooks, plugins, apps,
  memories and browser/computer tools disabled. It can still run read-only commands,
  including scripts in the target repository. A trusted project's `.codex`
  configuration still applies.
- The source endpoint only serves paths that resolve inside the repository; symlinks
  leading outside are refused.

## Cache

Everything is cached in `.code-xplr/` inside the explored directory (it contains its
own `.gitignore`, so git ignores it; `--cache-dir` puts it elsewhere, e.g. for
read-only checkouts).

A map's key is its scope plus the content of every file it covers (git blob hashes of
the working tree), the prompt version and the model. So:

- a component keeps its map across commits, branches and uncommitted edits that do
  not touch its files; committing changes you already had uncommitted invalidates
  nothing;
- when its files change, the last map is still shown, marked stale with the exact
  files added, removed or changed, until **Refresh** or the next `--index`;
- going back to earlier content (e.g. checking out an older branch) finds that
  content's map again.

Example inputs are cached per map content and flow. Delete `.code-xplr/` to start over.
