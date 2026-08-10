# cellar

[![Ler em Português](https://img.shields.io/badge/Ler%20em-Portugu%C3%AAs-009c3b?style=for-the-badge)](README_BR.md)

> Este documento também está disponível em [português](README_BR.md).

A searchable archive of Meta's shipped client source. Download a WhatsApp Web
revision, diff it against another, read any module, and trace what depends on what.

WhatsApp Web ships as tens of thousands of minified JavaScript chunks. Each chunk
packs many `__d("ModuleName", [deps], factory, id)` definitions onto a single line.
`cellar` downloads a client revision, parses it with the [oxc](https://oxc.rs) AST,
and writes one file per module, named after the module. The result is a directory
that `grep -r`, your editor, and your coding agent all work on directly.

## Features

- Download and manage source bundles for any past client revision.
- Diff two revisions through a named filter, in JSON, NDJSON, Markdown or text.
- Search modules by name, source text, or exported symbol.
- Print any module's source, dependencies, dependents and exports.
- Build dependency and dependent graphs, as JSON, DOT or Mermaid.
- Serve all of the above to a coding agent over MCP.
- Supports `whatsapp`, `messenger`, `facebook` and `instagram`.

## Requirements

- A recent stable Rust toolchain (1.95 or newer).
- Around 1.3 GB of disk per indexed revision.
- [`just`](https://github.com/casey/just) is optional, for the helper recipes.

## Installation

```bash
git clone https://github.com/polymorfa/cellar
cd cellar
cargo install --path crates/cellar
```

## Usage

### Managing bundles

Index the current WhatsApp Web revision. This downloads a few hundred megabytes and
takes a few minutes.

```bash
cellar bundle add --rev latest
cellar bundle add --rev 1030882912
```

Bundles are stored in `~/.cellar` by default. Set `CELLAR_HOME` to change that.

```bash
cellar bundle list
cellar bundle info latest
cellar bundle rm whatsapp-1030882912 --yes
```

You can also index an archive you already have, either a `.zip` or a directory of
extracted chunks.

```bash
cellar bundle import --rev 1030882912 --from ./whatsapp-1030882912
```

`cellar bundle info` prints extraction diagnostics. Check them before trusting a
surprising result. A non-zero `chunkParseFailures` means modules are missing from
the index rather than missing from WhatsApp.

### Searching modules

```bash
cellar module search --name '^WAWeb' --source 'addonType' -C 2
cellar grep 'disappearing_mode' --filter protocol
cellar module show WAWebSendMsgStanza
cellar module show WAWebSendMsgStanza --path-only
```

Narrow with `--name` whenever possible. The name pattern is applied to the in-memory
index first, so only the surviving files are opened.

### Diffing revisions

```bash
cellar diff --no-hunks
cellar diff whatsapp-1030882912 whatsapp-1044822804 --format json
```

With no arguments, `diff` compares the two newest stored revisions. Start with
`--no-hunks` for an overview, then re-run for the modules worth reading.

Changes whose every altered line is transpiler output are counted as `noiseOnly` in
the summary and left out of the change list. Pass `--include-noise` to see them.

### Dependency graphs

```bash
cellar graph WAWebSendMsgStanza --direction dependents --depth 2
cellar graph --match '^WAWebNewsletter' --direction deps --format mermaid
```

Minified call sites reference dependencies positionally (`d[3]`) instead of by name.
The dependency arrays are the only record of which module uses which, and `cellar`
inverts them at index time. Grep cannot answer this question.

## Filters

A bundle holds around 170,000 modules. The protocol surface is roughly 11% of that.
The rest is React components, analytics, and the other Meta products that share the
bundle. Without a filter, a diff between two revisions runs to tens of thousands of
entries.

| Filter | Keeps | Use for |
| --- | --- | --- |
| `default` | Protocol surface (~11%) | Release-to-release diffs |
| `all` | Everything | Checking whether the filter hid your answer |
| `protocol` | Positively identified protocol modules | Tight, high-signal diffs |
| `schemas` | `.pb`, `.proto` and `.graphql` | New protobuf fields, new persisted queries |
| `wam` | WAM analytics events and enums | Early traces of unreleased features |

The `default` filter hard-excludes `.pb` and `.graphql` modules, because they are
generated artifacts that a text diff reports badly. Use `--filter schemas` when
looking for a new protobuf field.

Inspect a filter before trusting it, then fork and edit it if needed.

```bash
cellar filter list
cellar filter test default --bundle latest
cellar filter fork default mine
cellar filter set mine --from ./mine.json
```

Precedence is `hardExclude`, then `include`, then `exclude`, then `defaultVerdict`.
With `excludeDependentsOfExcluded`, a module that transitively depends on an excluded
module is also excluded. That is computed as a fixpoint over the graph, so the result
does not depend on the order modules were visited.

## Agent integration

`cellar mcp` serves the same operations over MCP as sixteen tools.

```bash
just install-claude    # Claude Code
just install-codex     # Codex
just install-agents    # both
```

Each recipe installs the binary, registers the MCP server, and installs a skill that
tells the agent when to use it. Read-only tools are annotated as such, `bundle_remove`
requires explicit confirmation, and `bundle_add` is the only tool marked as reaching
the network.

Every result carries an absolute path, so an agent can start with a `cellar` query
and continue with its own file reading and grep.

## How it works

Meta serves a zip of the chunks for any past revision at
`https://www.facebook.com/btarchive/<revision>/<platform>`. No login is required.
The live bundle URLs stop resolving once a rollout moves on, so this endpoint is
what makes release-to-release diffing possible. `bundle add` is the only operation
that touches the network.

The endpoint does require a self-consistent browser request. Meta's edge checks the
`User-Agent` against the rest of the request, and a request claiming Chrome without
`Sec-Fetch-*` headers gets a 400. See `NAVIGATION_HEADERS` in `cellar-fetch` for the
measured status codes.

Some design notes:

- **AST, not regex.** Module boundaries come from the parser. Counting braces
  through string, template and regex literals truncates modules silently, and a
  truncated module shows up as a false diff on the next revision.
- **Deterministic output.** Every list is sorted and every map is a `BTreeMap`, so
  re-indexing a bundle produces byte-identical JSON. Chunks are parsed in parallel,
  and filename assignment runs serially over sorted names.
- **Diagnostics instead of silence.** Anything seen but unresolved is counted in
  `manifest.json`. Truncated results say so, and a skipped diff says why.
- **Sources are re-printed.** A module on one 200 KB line makes grep and line diffs
  useless, so modules are printed from the AST. Byte identity is tracked separately
  as `rawSha256`, so change detection stays exact.
- **Variants are kept.** About a quarter of modules ship with more than one distinct
  definition. All of them are written out, and change detection hashes the whole set.

## Project layout

| Crate | Role |
| --- | --- |
| `cellar-core` | Store layout, index model, filter engine, diff, graphs. No I/O beyond the filesystem. |
| `cellar-index` | oxc bundle parsing into a module index. |
| `cellar-fetch` | Revision discovery, download and unpacking. The only crate that reaches the network. |
| `cellar` | CLI and MCP server, both built on one shared operations layer. |

## Development

```bash
just ci        # fmt-check, clippy, tests, skill check
just test
just clippy
```

No test may reach the network. CI enforces this.

## Credits

- [ProtoCocktail](https://github.com/purpshell)'s `wa-diff-analyzer`, whose module
  filter ruleset is ported here as the `default` filter.
- [whatspec](https://github.com/oxidezap/whatspec) by João Lucas, which extracts a
  typed protocol IR from the same bundles and shaped the design here.
- [meta-code-verify](https://github.com/facebookincubator/meta-code-verify), Meta's
  own source-integrity extension, which documents the `btarchive` endpoint.

## Get support

If you'd like business to enterprise-level support from Rajeh, you can book a video
chat. Book a 1 hour time slot by contacting him on Discord or pre-ordering
[here](https://purpshell.dev/book). The earlier you pre-order the better, as his
time slots usually fill up very quickly.

If you are a business, we encourage you to contribute back to the development costs
of the project. You can do so by booking meetings or sponsoring below. All support
is welcome from businesses of all sizes.

## Sponsor

If you'd like to financially support this project, you can do so
[here](https://purpshell.dev/sponsor).

## Disclaimer

> [!CAUTION]
> This project is not affiliated, associated, authorized, endorsed by, or in any way
> officially connected with WhatsApp or any of its subsidiaries or its affiliates.
> The official WhatsApp website can be found at whatsapp.com. "WhatsApp" as well as
> related names, marks, emblems and images are registered trademarks of their
> respective owners.
>
> `cellar` reads publicly served client bundles for interoperability research. The
> maintainers do not condone the use of this project in practices that violate the
> Terms of Service of WhatsApp, and call upon the personal responsibility of its
> users to use it fairly.

## License

Copyright (c) 2026 Rajeh Taher

Licensed under the MIT License. See [LICENSE](LICENSE) for the full text.
