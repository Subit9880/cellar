---
name: cellar
description: Look things up in Meta's shipped client source (WhatsApp Web, Messenger, Facebook, Instagram) — find where a feature is implemented, discover unreleased features by diffing two client revisions, read any module's source, and trace what depends on what. Use when asked about WhatsApp Web internals, a WA protocol detail, what changed between two WhatsApp releases, where some behaviour lives in the client, or any question answerable from Meta's minified bundle. Triggers on "WhatsApp Web source", "what changed in this revision", "how does WhatsApp implement X", "which module does Y", "btarchive", "bundle diff", "WAWeb...", "WASmax...", "WAM event".
---

# cellar

`cellar` keeps an archive of Meta's shipped client bundles, one directory per
client revision, exploded so that **every module is its own ordinary file**. That
last part is the point: once a query points you at a module, you can read and grep
those files with your normal tools instead of going back through the CLI.

## Before anything else

```bash
cellar bundle list
```

Nothing in the archive means nothing to search. Adding a bundle takes several
minutes and gigabytes — say so before starting one, and never start two at once.

```bash
cellar bundle add --rev latest          # fetch and index the current revision
cellar bundle add --rev 1030882912      # a specific historical revision
```

Meta serves historical revisions from `btarchive`, which is what makes
release-to-release diffing possible at all — the live bundle URLs 404 the moment a
rollout moves on.

## Pick the right question

**"Where is X implemented?"** — search, then read the files.

```bash
cellar module search --source 'addon_type|addonType' --name '^WAWeb' -C 2
cellar grep 'disappearing_mode' --filter default
```

Narrow with `--name` whenever you can. It cuts the candidate set in the index
before any file is opened, which is the difference between opening forty files and
a hundred thousand. Then read the `path` each hit reports with your own file tools.

**"What changed in this release?"** — diff, overview first.

```bash
cellar diff --no-hunks                       # the two newest stored revisions
cellar diff whatsapp-1030882912 whatsapp-1030947950 --format json
```

Read the summary before the changes. `added` and `removed` modules are where new
features appear; `modified` with new dependencies or new exports is where existing
ones grow. Changes that are pure transpiler churn are counted as `noiseOnly` and
withheld — pass `--include-noise` only if you suspect the noise rules are wrong.

**"What uses this?"** — the graph, not grep.

```bash
cellar graph WAWebSendMsgStanza --direction dependents --depth 2
cellar graph --match '^WAWebNewsletter' --direction deps --depth 1 --format mermaid
```

Grep genuinely cannot answer this. Minified call sites reference modules
positionally (`d[3]`), never by name, so the only record of "who uses this" is the
dependency arrays the indexer inverted.

**"Show me this module."**

```bash
cellar module show WAWebSendMsgStanza
cellar module show WAWebSendMsgStanza --path-only    # then read it directly
```

## Filters decide what you are looking at

A bundle holds ~170k modules, most of them React components and code for the other
Meta products that share the bundle. Every operation takes `--filter`:

| filter | keeps | use for |
| --- | --- | --- |
| `default` | ~11% — the protocol surface | diffing releases; the default |
| `all` | everything | when you suspect the filter hid your answer |
| `protocol` | only positively-identified protocol modules | a tight, high-signal diff |
| `schemas` | `.pb` / `.proto` / `.graphql` | new protobuf fields, new persisted queries |
| `wam` | WAM analytics events and enums | earliest traces of unreleased features |

`default` **hard-excludes** `.pb` and `.graphql` modules. If you are looking for a
new protobuf field, use `--filter schemas` — otherwise you will conclude, wrongly,
that nothing changed.

Check an unfamiliar filter before trusting it:

```bash
cellar filter test default --bundle latest
```

Built-ins are read-only. To adjust one, fork it:

```bash
cellar filter fork default my-filter
cellar filter show my-filter > /tmp/f.json     # edit, then:
cellar filter set my-filter --from /tmp/f.json
```

Precedence is `hardExclude`, then `include`, then `exclude`; anything unmatched
takes `defaultVerdict`. With `excludeDependentsOfExcluded`, a module that depends
on an excluded one is excluded too — unless `include` claimed it.

## Reading results honestly

- **Diagnostics are not decoration.** `cellar bundle info <bundle>` reports parse
  failures and unresolved dependencies. A surprising diff with non-zero
  `chunkParseFailures` means modules are genuinely missing from the index, not that
  they were removed from the client.
- **Absent is not zero.** Truncated results say `truncated: true`; a skipped diff
  says `hunksOmitted` and why. Never read a short list as a complete one.
- **Variants are real.** About a quarter of modules ship in more than one form.
  `variants` lists the others, written beside the primary as
  `<name>~alt-<hash>.js`. A variant appearing or disappearing between revisions is
  itself a rollout signal.
- **Stored sources are re-printed, not byte-exact.** Modules are pretty-printed so
  grep and diff work line by line. Identity is tracked separately by `rawSha256`,
  so change detection is still exact.

## MCP

The same operations are available as MCP tools (`cellar mcp`), named
`bundle_*`, `filter_*`, `module_get`, `module_search`, `diff`, `graph`. Prefer them
when available; fall back to the CLI otherwise. Both return identical JSON.

## Reporting findings

Cite the module name and its path, and quote the lines you are relying on. "New
`WAWebFooBar` module, `modules/WAWebFooBar.js:41`, sends `<iq type='set'
xmlns='w:foo'>`" is a finding. "It looks like they added foo support" is a guess —
if that is all you have, say so.
