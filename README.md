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
./target/release/code-explore /path/to/repo        # then open http://127.0.0.1:7878
```

Options: `--port`, `--host` (keep loopback; the API serves repository files),
`--model`, `--codex <path>`, `--timeout-mins` (per exploration or question, default 20),
`--cache-dir`, `--lsp LANG=COMMAND` (repeatable), `--no-lsp`.

Opening the UI starts the system exploration if no map is cached. That usually takes
a few minutes on a mid-size repository; progress shows each command Codex runs.

## Using the map

- Click a box or arrow: summary, paths, interactions and evidence in the side panel.
- Double-click a box, or **Explore inside**: map that component one level deeper.
  Breadcrumbs at the top go back up.
- Click a data flow: its steps are highlighted and numbered on the map.
- Click a citation or path: source viewer at that line.
- Drag a box to move it when labels overlap; edges follow. Positions are saved per
  map (repository, commit, level) in browser storage. **Reset layout** discards them.
- Mouse wheel zooms, dragging the background pans, `F` fits, `Esc` clears the selection.

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

Maps are stored under `$XDG_CACHE_HOME/code-explore/<repo>-<hash>/<commit>/`, keyed
by `HEAD` at startup and scope. Restart after checking out another commit.
Uncommitted changes at exploration time are flagged in the map. **Re-explore**
replaces the cached map for the current level; deeper maps keep their cached version
until re-explored.
