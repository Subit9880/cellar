# cellar

**An archive of Meta's shipped client source that you — or an agent — can look
things up in.** Download a client revision, diff it against another, read any
module, and trace what depends on what.

WhatsApp Web (and Messenger, Facebook, Instagram) ship as a few tens of thousands
of minified JavaScript chunks, each holding many
`__d("ModuleName", [deps], factory, id)` definitions. `cellar` downloads a
revision, parses it with the [`oxc`](https://oxc.rs) AST, and writes **one file per
module, named after the module** — so the archive is something `grep -r` and an
ordinary text editor work on directly.

```
$CELLAR_HOME/                        (default: ~/.cellar)
  filters/<name>.json                user filters; built-ins are compiled in
  bundles/whatsapp-1030882912/
    manifest.json                    identity, integrity, counts, diagnostics
    index.json                       every module, sorted by name
    modules/
      WAWebSendMsgStanza.js          one file per module
      WAWebSendMsgStanza~alt-….js    a second definition, where one ships
```

## Quick start

```bash
cargo install --path crates/cellar     # or: just install

# Fetch and index the current WhatsApp Web revision (minutes; gigabytes).
cellar bundle add --rev latest

# …or index an archive you already have (a .zip, or a directory of chunks).
cellar bundle import --rev 1030882912 --from ./whatsapp-1030882912

# What changed between the two newest revisions?
cellar diff --no-hunks

# Where is this implemented?
cellar module search --name '^WAWeb' --source 'addonType' -C 2

# What uses this?  (grep cannot answer this — see below.)
cellar graph WAWebSendMsgStanza --direction dependents --depth 2

# Serve all of the above to an agent over MCP.
cellar mcp
```

Every command that produces data takes `--json`; `diff` also does
`--format ndjson|md`, and `graph` does `--format dot|mermaid`.

### What it looks like

Diffing two consecutive WhatsApp Web revisions through the `protocol` filter —
4,715 modules of the 172,198 in the bundle — takes about a second and turns up
things like a new `WAWebSettingsSync*` module family carrying
`SyncActionValue$SettingsSyncAction$SettingKey` with 23 keys (`appTheme`,
`wallpaperId`, notification tones, autodownload flags), wired into app-state sync
via `WAWebCollectionHandlerActions`. That is cross-device settings sync, visible
in the client before it is announced.

## Why not just grep the bundle?

You can, and for some questions you should — that is what the `modules/`
directory is for. Two questions it cannot answer:

- **"What uses this module?"** Minified call sites reference dependencies
  positionally (`d[3]`), never by name. The only record of the reverse edge is the
  dependency arrays, which `cellar` inverts at index time.
- **"What changed since last release?"** Meta serves only the current bundle; the
  URLs 404 as soon as a rollout moves on. Historical revisions come from
  `btarchive` (see below), which is what makes release-to-release diffing possible
  at all.

## Where the bundles come from

`https://www.facebook.com/btarchive/<revision>/<platform>` serves a zip of the
chunks for any past revision, for `whatsapp`, `messenger`, `facebook` and
`instagram`. No login required.

It does require looking like a browser, in a specific way: Meta's edge
cross-checks the `User-Agent` against the rest of the request, and a request
claiming Chrome while omitting `Sec-Fetch-*` is answered `400`. `cellar` sends the
full navigation header set — see `NAVIGATION_HEADERS` in `cellar-fetch`, which
documents the measured status codes.

`bundle add` is the only operation that touches the network.

## Filters

A bundle holds ~170k modules; the protocol surface is about 11% of it, and the rest
is React components, analytics, and the other Meta products sharing the bundle. A
filter is what makes a diff readable instead of 40,000 entries long.

| filter | keeps | for |
| --- | --- | --- |
| `default` | protocol surface (~11%) | release-to-release diffs |
| `all` | everything | checking whether the filter hid something |
| `protocol` | positively-identified protocol modules only | tight, high-signal diffs |
| `schemas` | `.pb` / `.proto` / `.graphql` | new protobuf fields, new persisted GraphQL |
| `wam` | WAM analytics events and enums | early traces of unreleased features |

Precedence is `hardExclude`, then `include`, then `exclude`, then
`defaultVerdict`. With `excludeDependentsOfExcluded`, a module that transitively
depends on an excluded one is excluded too — computed as a fixpoint over the
graph, so the result does not depend on the order modules were visited.

```bash
cellar filter test default --bundle latest    # what does it actually keep?
cellar filter fork default mine               # built-ins are read-only
cellar filter set mine --from ./mine.json     # partial documents are edits
```

## Design

- **AST, not regex.** Module boundaries come from the parser. Brace-counting
  through string, template and regex literals silently truncates modules, and a
  truncated module reads as a spurious diff on the next revision. Exports resolve
  through the factory's actual parameter binding, so a minified
  `q.sendStanza = …` is found where a `e\.(\w+)=` pattern finds nothing.
- **Deterministic.** Every list is sorted and every map is a `BTreeMap`, so
  re-indexing the same bundle produces byte-identical JSON. Chunks are parsed in
  parallel, but filename assignment — the only order-sensitive step — runs
  serially over sorted names.
- **Diagnostics, not silence.** Anything seen but not resolved is counted in
  `manifest.json` rather than dropped, so "this bundle has no such module" and "we
  failed to extract it" never look alike. Truncated results say so; a skipped diff
  says why.
- **Sources are re-printed.** The bundle ships each module on one enormous line,
  which makes a greppable directory useless and a line diff meaningless. Modules
  are printed from the AST instead. Byte-level identity is preserved separately as
  `rawSha256`, so change detection stays exact.
- **Variants are kept.** About a quarter of modules ship with more than one
  distinct definition (a build compiled against `react-compiler-runtime` alongside
  one that is not, for example). All are written out, and change detection hashes
  the whole set — otherwise a build variant appearing between revisions would read
  as no change, and a shift in which one was picked would read as a change that
  never happened.

## Agent integration

```bash
just install-agents     # binary + MCP registration + skill, Claude Code and Codex
```

Sixteen MCP tools mirroring the CLI, tools-only JSON-RPC over stdio. Read-only
tools are annotated as such, `bundle_remove` requires explicit confirmation, and
`bundle_add` is the only one marked `openWorldHint`. Tool failures come back as
`isError: true` with the message, so the model can read and adjust rather than
having the client swallow a transport error.

Every result carries the module's absolute path, because the expected next step is
for the agent to take over with its own file-reading and grep tools.

The skill lives at `.claude/skills/cellar/SKILL.md`; `.codex/skills/cellar` is a
symlink to it, so the two agents cannot drift apart.

## Layout

| crate | role |
| --- | --- |
| `cellar-core` | store layout, index model, filter engine, diff, graphs. No I/O beyond the filesystem. |
| `cellar-index` | oxc bundle parsing → module index. Native only. |
| `cellar-fetch` | revision discovery, `btarchive` download, unpacking. The only crate that reaches the network. |
| `cellar` | CLI and MCP server, both thin shells over one shared operations layer. |

Builds on stable Rust 1.95+ (the floor is set by `oxc`). There is no
`rust-toolchain.toml`, so it uses whatever toolchain you have.

## Prior art

- **[ProtoCocktail](https://github.com/purpshell)**'s `wa-diff-analyzer`, whose
  module-filter ruleset — several hundred patterns accumulated empirically against
  real bundles — is ported here as the `default` filter. That ruleset is worth more
  than the code around it, and `cellar` reproduces its judgement calls faithfully;
  the differences are structural, and documented in `cellar-core/src/builtin.rs`.
- **[whatspec](https://github.com/oxidezap/whatspec)** by João Lucas, which solves
  the adjacent problem of extracting a typed protocol IR from the same bundles. Its
  approach — oxc over regex, deterministic output, diagnostics rather than silent
  drops — shaped the design here. If you want a structured protocol contract rather
  than a searchable archive, use whatspec.
- **[meta-code-verify](https://github.com/facebookincubator/meta-code-verify)**,
  Meta's own source-integrity extension, which is where the `btarchive` endpoint
  and the shape of a request it accepts are documented in practice.

## Contributing

`just ci` runs everything CI does: `fmt-check`, `clippy -D warnings`, the test
suite, and the skill-consistency check. No test may reach the network.

## Disclaimer

`cellar` is an independent tool and is **not affiliated with, endorsed by, or
sponsored by WhatsApp LLC or Meta**. "WhatsApp" is a trademark of its respective
owner and is used here only descriptively, to identify the software this tool
reads. It reads publicly served client bundles for interoperability research.

## License

MIT — see [LICENSE](LICENSE).
