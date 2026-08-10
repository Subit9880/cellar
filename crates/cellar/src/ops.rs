//! Every operation the tool can perform, as one implementation.
//!
//! The CLI and the MCP server are both thin shells over this module. That is
//! deliberate: an agent driving the MCP server and a person driving the CLI must
//! see the same answers, and a capability that exists in one and not the other is
//! the usual way these tools rot.
//!
//! Operations return [`serde_json::Value`], because JSON is the contract for the
//! MCP side and the CLI renders from it. Where a human-readable rendering differs
//! meaningfully from the data, the CLI does the rendering — never a second query.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use cellar_core::builtin;
use cellar_core::diff::{DiffOptions, diff_bundles};
use cellar_core::filter::{Filter, FilterSet};
use cellar_core::graph::{self, Direction, GraphOptions};
use cellar_core::model::{BundleId, BundleManifest, ModuleIndex, Platform, SCHEMA_VERSION};
use cellar_core::search::{self, Query};
use cellar_core::store::{BundleHandle, Store};
use cellar_index::{IndexOptions, index_directory};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

/// How many near-miss names to offer when an exact module lookup fails.
const SUGGESTION_LIMIT: usize = 10;

/// Shared state for a run: the archive, plus a per-process cache of parsed indexes.
///
/// The cache matters for the MCP server, which stays up across many calls and would
/// otherwise re-parse a 60 MB `index.json` for every question asked.
pub struct Ctx {
    pub store: Store,
    indexes: RefCell<HashMap<BundleId, Rc<ModuleIndex>>>,
    /// Where progress goes. The MCP server must keep stdout clean for JSON-RPC, so
    /// this is always stderr.
    quiet: bool,
}

impl Ctx {
    pub fn new(store: Store, quiet: bool) -> Self {
        Self {
            store,
            indexes: RefCell::new(HashMap::new()),
            quiet,
        }
    }

    fn progress(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }

    /// Resolve a bundle spec and load its index, caching the parse.
    pub fn index_of(&self, spec: &str) -> Result<(BundleHandle, Rc<ModuleIndex>)> {
        let id = self.store.resolve(spec)?;
        let handle = self.store.open_bundle(id)?;
        if let Some(cached) = self.indexes.borrow().get(&id) {
            return Ok((handle, Rc::clone(cached)));
        }
        let index = Rc::new(handle.index()?);
        self.indexes.borrow_mut().insert(id, Rc::clone(&index));
        Ok((handle, index))
    }

    /// Load a filter, defaulting to `default` when unnamed.
    pub fn filter(&self, name: Option<&str>) -> Result<FilterSet> {
        self.store.compiled_filter(name.unwrap_or("default"))
    }

    // --- bundles -----------------------------------------------------------

    /// Download (or import) a bundle and index it.
    pub fn bundle_add(
        &self,
        platform: Platform,
        revision: Option<u64>,
        from: Option<&Path>,
        pretty: bool,
        force: bool,
        keep_chunks: bool,
    ) -> Result<Value> {
        let revision = match revision {
            Some(r) => r,
            None => {
                self.progress("resolving the current revision…");
                let r = cellar_fetch::latest_revision(platform)?;
                self.progress(&format!("  current {platform} revision is {r}"));
                r
            }
        };
        let id = BundleId::new(platform, revision);

        if self.store.has_bundle(id) && !force {
            bail!("bundle {id} is already in the archive; pass --force to re-index it");
        }

        // Preparing the bundle directory deletes everything under it, so a source
        // that lives inside it would be destroyed before it could be read. The
        // reachable way to hit this is pointing `--from` at a bundle's own kept
        // `.chunks`; refuse and name the command that does that safely.
        if let Some(path) = from
            && is_inside(path, &self.store.bundle_dir(id))
        {
            bail!(
                "--from {} is inside the bundle directory that indexing would clear first; \
                 use `cellar bundle reindex {id}` to re-index from kept chunks",
                path.display()
            );
        }

        let bundle_dir = self.store.begin_bundle(id)?;
        let chunks_dir = cellar_fetch::staging_dir(&bundle_dir);

        // Provenance is recorded only when it is actually known: an imported
        // directory has no archive to hash and no URL it came from, and inventing
        // either would make the manifest lie.
        let mut source_url = None;
        let mut archive_sha256 = None;
        let mut archive_len = None;

        match from {
            Some(path) if path.is_dir() => {
                self.progress(&format!("importing chunks from {}", path.display()));
                chunks_dir_from_existing(path, &chunks_dir)?;
            }
            Some(path) => {
                self.progress(&format!("unpacking {}", path.display()));
                let (sha, len) = cellar_fetch::hash_file(path)?;
                std::fs::create_dir_all(&chunks_dir)?;
                cellar_fetch::extract_zip(path, &chunks_dir)?;
                archive_sha256 = Some(sha);
                archive_len = Some(len);
            }
            None => {
                let url = cellar_fetch::archive_url(id);
                self.progress(&format!("downloading {url}"));
                let fetched = cellar_fetch::download_and_extract(id, &chunks_dir)?;
                self.progress(&format!(
                    "  {} bytes, {} archive members",
                    fetched.len, fetched.members
                ));
                source_url = Some(fetched.url);
                archive_sha256 = Some(fetched.sha256);
                archive_len = Some(fetched.len);
            }
        }

        self.progress("indexing…");
        let outcome = index_directory(&chunks_dir, &bundle_dir, id, &IndexOptions { pretty })?;

        if keep_chunks {
            self.progress(&format!("  chunks kept at {}", chunks_dir.display()));
        } else {
            std::fs::remove_dir_all(&chunks_dir).ok();
        }

        let manifest = BundleManifest {
            schema_version: SCHEMA_VERSION.to_string(),
            bundle: id,
            source_url,
            archive_sha256,
            archive_len,
            indexed_at: chrono::Utc::now().to_rfc3339(),
            cellar_version: env!("CARGO_PKG_VERSION").to_string(),
            codegen_version: cellar_index::CODEGEN_VERSION.to_string(),
            source_form: outcome.source_form,
            modules_indexed: outcome.index.len() as u64,
            modules_bytes: outcome.modules_bytes,
            diagnostics: outcome.diagnostics,
        };

        let handle = self.store.commit_bundle(id, &outcome.index, &manifest)?;
        self.indexes.borrow_mut().remove(&id);
        self.progress(&format!(
            "  indexed {} modules into {}",
            manifest.modules_indexed,
            handle.modules_dir().display()
        ));

        Ok(json!({
            "bundle": id.to_string(),
            "dir": handle.dir.display().to_string(),
            "modulesDir": handle.modules_dir().display().to_string(),
            "manifest": manifest,
        }))
    }

    /// Re-index a bundle from the chunk files kept by `--keep-chunks`.
    ///
    /// The chunks are moved out of the bundle directory before it is cleared and
    /// moved back afterwards, which is the whole reason this is a separate command
    /// rather than an `import --from .../.chunks`: that would delete its own input.
    pub fn bundle_reindex(&self, spec: &str, pretty: bool, keep_chunks: bool) -> Result<Value> {
        let id = self.store.resolve(spec)?;
        let handle = self.store.open_bundle(id)?;
        let kept = cellar_fetch::staging_dir(&handle.dir);
        if !kept.is_dir() {
            bail!(
                "bundle {id} has no kept chunk files at {}; re-add it with \
                 `cellar bundle add --rev {} --force --keep-chunks`, or re-download with \
                 `cellar bundle add --rev {} --force`",
                kept.display(),
                id.revision,
                id.revision
            );
        }

        // Park the chunks outside the directory about to be wiped. A sibling keeps
        // it on the same filesystem, so this is a rename rather than a 1.5 GB copy.
        let parked = self.store.bundles_dir().join(format!(".reindex-{id}"));
        if parked.exists() {
            std::fs::remove_dir_all(&parked)?;
        }
        std::fs::rename(&kept, &parked)
            .with_context(|| format!("parking kept chunks at {}", parked.display()))?;

        let previous = handle.manifest.clone();
        let result = (|| -> Result<Value> {
            let bundle_dir = self.store.begin_bundle(id)?;
            self.progress("indexing…");
            let outcome = index_directory(&parked, &bundle_dir, id, &IndexOptions { pretty })?;

            let manifest = BundleManifest {
                schema_version: SCHEMA_VERSION.to_string(),
                // Provenance carries over: it is still the same bytes from the same
                // place, only re-read. Dropping it would lose the only record of
                // where the bundle came from.
                source_url: previous.source_url.clone(),
                archive_sha256: previous.archive_sha256.clone(),
                archive_len: previous.archive_len,
                bundle: id,
                indexed_at: chrono::Utc::now().to_rfc3339(),
                cellar_version: env!("CARGO_PKG_VERSION").to_string(),
                codegen_version: cellar_index::CODEGEN_VERSION.to_string(),
                source_form: outcome.source_form,
                modules_indexed: outcome.index.len() as u64,
                modules_bytes: outcome.modules_bytes,
                diagnostics: outcome.diagnostics,
            };
            let handle = self.store.commit_bundle(id, &outcome.index, &manifest)?;
            self.indexes.borrow_mut().remove(&id);
            Ok(json!({
                "bundle": id.to_string(),
                "dir": handle.dir.display().to_string(),
                "modulesDir": handle.modules_dir().display().to_string(),
                "manifest": manifest,
            }))
        })();

        // Put the chunks back (or drop them) whether or not indexing succeeded — a
        // failed re-index must not also lose the inputs needed to retry.
        if keep_chunks {
            let restored = cellar_fetch::staging_dir(&self.store.bundle_dir(id));
            if let Some(parent) = restored.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::rename(&parked, &restored).ok();
        } else {
            std::fs::remove_dir_all(&parked).ok();
        }

        result
    }

    pub fn bundle_list(&self) -> Result<Value> {
        let bundles: Vec<Value> = self
            .store
            .list_bundles()?
            .into_iter()
            .map(|h| {
                json!({
                    "bundle": h.id.to_string(),
                    "platform": h.id.platform.as_str(),
                    "revision": h.id.revision,
                    "modules": h.manifest.modules_indexed,
                    "indexedAt": h.manifest.indexed_at,
                    "sourceForm": h.manifest.source_form,
                    "dir": h.dir.display().to_string(),
                    "modulesDir": h.modules_dir().display().to_string(),
                })
            })
            .collect();
        Ok(json!({ "root": self.store.root().display().to_string(), "bundles": bundles }))
    }

    pub fn bundle_info(&self, spec: &str) -> Result<Value> {
        let id = self.store.resolve(spec)?;
        let handle = self.store.open_bundle(id)?;
        Ok(json!({
            "bundle": id.to_string(),
            "dir": handle.dir.display().to_string(),
            "modulesDir": handle.modules_dir().display().to_string(),
            "indexPath": handle.index_path().display().to_string(),
            "manifest": handle.manifest,
        }))
    }

    pub fn bundle_remove(&self, spec: &str) -> Result<Value> {
        let id = self.store.resolve(spec)?;
        self.store.remove_bundle(id)?;
        self.indexes.borrow_mut().remove(&id);
        Ok(json!({ "removed": id.to_string() }))
    }

    // --- filters -----------------------------------------------------------

    pub fn filter_list(&self) -> Result<Value> {
        let filters: Vec<Value> = self
            .store
            .list_filters()?
            .into_iter()
            .map(|f| {
                json!({
                    "name": f.name,
                    "description": f.description,
                    "builtin": f.builtin,
                    "defaultVerdict": f.default_verdict,
                    "hardExclude": f.hard_exclude.len(),
                    "include": f.include.len(),
                    "exclude": f.exclude.len(),
                    "excludeDependentsOfExcluded": f.exclude_dependents_of_excluded,
                })
            })
            .collect();
        Ok(json!({ "filters": filters, "dir": self.store.filters_dir().display().to_string() }))
    }

    pub fn filter_get(&self, name: &str) -> Result<Value> {
        Ok(serde_json::to_value(self.store.get_filter(name)?)?)
    }

    pub fn filter_put(&self, mut filter: Filter, name: Option<&str>) -> Result<Value> {
        if let Some(n) = name {
            filter.name = n.to_string();
        }
        let name = filter.name.clone();
        self.store.put_filter(filter)?;
        Ok(json!({
            "saved": name,
            "path": self.store.filters_dir().join(format!("{name}.json")).display().to_string(),
        }))
    }

    pub fn filter_delete(&self, name: &str) -> Result<Value> {
        self.store.delete_filter(name)?;
        Ok(json!({ "deleted": name }))
    }

    pub fn filter_fork(&self, source: &str, dest: &str) -> Result<Value> {
        let mut filter = self.store.get_filter(source)?;
        filter.name = dest.to_string();
        filter.builtin = false;
        filter.description = format!("Forked from `{source}`. {}", filter.description);
        self.filter_put(filter, None)
    }

    /// Apply a filter to a bundle and report what it keeps.
    pub fn filter_test(&self, name: &str, bundle: &str, sample: usize) -> Result<Value> {
        let (_, index) = self.index_of(bundle)?;
        let set = self.store.compiled_filter(name)?;
        let decision = set.apply(&index);

        let kept: Vec<&String> = decision.kept.iter().take(sample).collect();
        let dropped: Vec<Value> = decision
            .reasons
            .iter()
            .filter(|(_, (v, _))| *v == cellar_core::Verdict::Drop)
            .take(sample)
            .map(|(name, (_, why))| json!({ "module": name, "why": why }))
            .collect();

        Ok(json!({
            "filter": name,
            "bundle": index.bundle.to_string(),
            "total": decision.total,
            "kept": decision.kept_count(),
            "dropped": decision.dropped_count(),
            "dropBreakdown": decision.drop_breakdown(),
            "keptSample": kept,
            "droppedSample": dropped,
        }))
    }

    // --- modules -----------------------------------------------------------

    /// One module: its metadata and, unless suppressed, its source.
    pub fn module_get(
        &self,
        bundle: &str,
        name: &str,
        start_line: u32,
        max_lines: Option<u32>,
        include_source: bool,
    ) -> Result<Value> {
        let (handle, index) = self.index_of(bundle)?;
        let Some(entry) = index.get(name) else {
            let suggestions = suggest(&index, name);
            bail!(
                "no module {name:?} in {}{}",
                index.bundle,
                if suggestions.is_empty() {
                    String::new()
                } else {
                    format!("\n  did you mean: {}", suggestions.join(", "))
                }
            );
        };
        let path = handle.module_path(entry);
        let mut out = json!({
            "bundle": index.bundle.to_string(),
            "module": entry,
            "path": path.display().to_string(),
        });
        if include_source {
            let source = search::read_lines(&path, start_line, max_lines)?;
            out["source"] = json!(source);
            out["startLine"] = json!(start_line);
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn module_search(
        &self,
        bundle: &str,
        name: Option<&str>,
        source: Option<&str>,
        exports: Option<&str>,
        filter: Option<&str>,
        limit: usize,
        context_lines: usize,
        max_matches: usize,
    ) -> Result<Value> {
        let (handle, index) = self.index_of(bundle)?;
        let q = Query {
            name: compile_opt(name, "name")?,
            source: compile_opt(source, "source")?,
            exports: compile_opt(exports, "exports")?,
            filter: match filter {
                Some(f) => Some(self.store.compiled_filter(f)?),
                None => None,
            },
            limit,
            context_lines,
            max_matches_per_module: max_matches,
        };
        let result = search::search(&handle, &index, &q)?;
        Ok(serde_json::to_value(result)?)
    }

    // --- diff --------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn diff(
        &self,
        old: &str,
        new: &str,
        filter: Option<&str>,
        opts: &DiffOptions,
        limit: Option<usize>,
    ) -> Result<Value> {
        let (old_handle, old_index) = self.index_of(old)?;
        let (new_handle, new_index) = self.index_of(new)?;
        if old_index.bundle == new_index.bundle {
            bail!(
                "old and new resolve to the same bundle ({}); nothing to diff",
                old_index.bundle
            );
        }
        let set = self.filter(filter)?;

        let mut result =
            diff_bundles(&old_index, &old_handle, &new_index, &new_handle, &set, opts)?;

        let mut value = serde_json::to_value(&result)?;
        if let Some(limit) = limit
            && result.changes.len() > limit
        {
            let total = result.changes.len();
            result.changes.truncate(limit);
            value = serde_json::to_value(&result)?;
            // The summary still reports the true counts; this only says the list
            // itself was cut, so a caller never reads a short list as a full one.
            value["changesTruncated"] = json!(true);
            value["changesTotal"] = json!(total);
        }
        value["oldDir"] = json!(old_handle.dir.display().to_string());
        value["newDir"] = json!(new_handle.dir.display().to_string());
        Ok(value)
    }

    /// The two newest bundles for a platform, as diff operands.
    pub fn default_diff_pair(&self, platform: Platform) -> Result<(String, String)> {
        let (old, new) = self.store.latest_pair(platform)?;
        Ok((old.to_string(), new.to_string()))
    }

    // --- graph -------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn graph(
        &self,
        bundle: &str,
        roots: &[String],
        root_pattern: Option<&str>,
        direction: Direction,
        depth: Option<usize>,
        max_nodes: usize,
        include_external: bool,
        detect_cycles: bool,
        filter: Option<&str>,
    ) -> Result<Value> {
        let (_, index) = self.index_of(bundle)?;

        let mut all_roots: Vec<String> = roots.to_vec();
        if let Some(pattern) = root_pattern {
            let re = Regex::new(pattern)
                .with_context(|| format!("--match {pattern:?} is not a valid regex"))?;
            all_roots.extend(
                index
                    .modules
                    .iter()
                    .filter(|m| re.is_match(&m.name))
                    .map(|m| m.name.clone()),
            );
        }
        all_roots.sort();
        all_roots.dedup();
        if all_roots.is_empty() {
            bail!("no roots: pass module names, or --match <regex> that selects some");
        }

        let set = match filter {
            Some(f) => Some(self.store.compiled_filter(f)?),
            None => None,
        };
        let opts = GraphOptions {
            direction,
            depth,
            max_nodes,
            include_external,
            detect_cycles,
        };
        let g = graph::build(&index, &all_roots, &opts, set.as_ref());
        Ok(serde_json::to_value(g)?)
    }
}

/// Whether `path` is `dir` or lives under it.
///
/// Compares canonicalized paths so that `..` segments and symlinks cannot be used
/// to present an inside path as an outside one. A path that cannot be canonicalized
/// does not exist yet, and so is not inside anything.
fn is_inside(path: &Path, dir: &Path) -> bool {
    let (Ok(path), Ok(dir)) = (path.canonicalize(), dir.canonicalize()) else {
        return false;
    };
    path.starts_with(&dir)
}

/// Compile an optional regex, naming which argument was wrong.
fn compile_opt(pattern: Option<&str>, field: &str) -> Result<Option<Regex>> {
    match pattern {
        None => Ok(None),
        Some(p) => Regex::new(p)
            .map(Some)
            .with_context(|| format!("{field} pattern {p:?} is not a valid regex")),
    }
}

/// Names similar to `wanted`, for a failed exact lookup. Substring matching in
/// both directions catches the two common mistakes: a remembered prefix, and a
/// name with one segment too many.
fn suggest(index: &ModuleIndex, wanted: &str) -> Vec<String> {
    let needle = wanted.to_ascii_lowercase();
    index
        .modules
        .iter()
        .filter(|m| {
            let name = m.name.to_ascii_lowercase();
            name.contains(&needle) || needle.contains(&name)
        })
        .take(SUGGESTION_LIMIT)
        .map(|m| m.name.clone())
        .collect()
}

/// Populate a staging directory from an already-extracted bundle directory.
///
/// Symlinks rather than copies where the platform allows it: these directories run
/// to ~1.5 GB, and an import should not duplicate that just to read it once. Falls
/// back to copying if linking fails.
fn chunks_dir_from_existing(source: &Path, staging: &Path) -> Result<()> {
    std::fs::create_dir_all(staging)?;
    let entries =
        std::fs::read_dir(source).with_context(|| format!("reading {}", source.display()))?;
    let mut linked = 0u64;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let dest = staging.join(entry.file_name());
        #[cfg(unix)]
        let result = std::os::unix::fs::symlink(entry.path(), &dest);
        #[cfg(not(unix))]
        let result: std::io::Result<()> = Err(std::io::Error::other("no symlinks"));
        if result.is_err() {
            std::fs::copy(entry.path(), &dest)?;
        }
        linked += 1;
    }
    if linked == 0 {
        bail!(
            "{} contains no files to import — expected a directory of extracted bundle chunks",
            source.display()
        );
    }
    Ok(())
}

/// A filter as accepted from JSON on the CLI or over MCP.
///
/// Separate from [`Filter`] so that every field is optional and a partial document
/// is a legal edit: `{"exclude": ["^Foo"]}` applied to an existing filter changes
/// only that list.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilterPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub hard_exclude: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub default_verdict: Option<cellar_core::Verdict>,
    pub exclude_dependents_of_excluded: Option<bool>,
    pub noise_deps: Option<Vec<String>>,
    pub noise_code: Option<Vec<String>>,
    /// Accepted and ignored, so a document read back from `filter show` can be fed
    /// straight into `filter set` without hand-editing. Never read: a filter
    /// written through this path is a user filter by definition.
    #[serde(default)]
    #[allow(dead_code)]
    pub builtin: Option<bool>,
}

impl FilterPatch {
    /// Apply this patch over `base`.
    pub fn apply(self, mut base: Filter) -> Filter {
        if let Some(v) = self.name {
            base.name = v;
        }
        if let Some(v) = self.description {
            base.description = v;
        }
        if let Some(v) = self.hard_exclude {
            base.hard_exclude = v;
        }
        if let Some(v) = self.include {
            base.include = v;
        }
        if let Some(v) = self.exclude {
            base.exclude = v;
        }
        if let Some(v) = self.default_verdict {
            base.default_verdict = v;
        }
        if let Some(v) = self.exclude_dependents_of_excluded {
            base.exclude_dependents_of_excluded = v;
        }
        if let Some(v) = self.noise_deps {
            base.noise_deps = v;
        }
        if let Some(v) = self.noise_code {
            base.noise_code = v;
        }
        base.builtin = false;
        base
    }
}

/// An empty filter to patch onto when creating one from scratch.
pub fn blank_filter(name: &str) -> Filter {
    Filter {
        name: name.to_string(),
        description: String::new(),
        hard_exclude: Vec::new(),
        include: Vec::new(),
        exclude: Vec::new(),
        default_verdict: cellar_core::Verdict::Keep,
        exclude_dependents_of_excluded: false,
        noise_deps: builtin::default().noise_deps,
        noise_code: builtin::default().noise_code,
        builtin: false,
    }
}

/// Read a JSON document from a file, or from stdin when the path is `-`.
pub fn read_json_arg(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading the filter document from stdin")?;
        return Ok(s);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Expand `~` in a user-supplied path, which shells do not do inside quoted args.
pub fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else {
        return path.to_path_buf();
    };
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_patch_changes_only_the_fields_it_names() {
        let base = builtin::default();
        let original_include = base.include.clone();
        let patch: FilterPatch = serde_json::from_str(r#"{"exclude": ["^Only$"]}"#).unwrap();
        let out = patch.apply(base);
        assert_eq!(out.exclude, ["^Only$"]);
        assert_eq!(out.include, original_include, "untouched lists survive");
        assert!(!out.builtin, "a patched built-in becomes a user filter");
    }

    #[test]
    fn a_filter_read_back_from_show_can_be_fed_straight_to_set() {
        let shown = serde_json::to_string(&builtin::default()).unwrap();
        let patch: FilterPatch = serde_json::from_str(&shown).expect("round-trips");
        let out = patch.apply(blank_filter("copy"));
        assert_eq!(out.include, builtin::default().include);
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_silently_ignored() {
        // A typo'd key would otherwise produce a filter that quietly does nothing
        // like what was asked for.
        let err = serde_json::from_str::<FilterPatch>(r#"{"exclud": ["x"]}"#).unwrap_err();
        assert!(err.to_string().contains("exclud"), "{err}");
    }

    #[test]
    fn suggestions_catch_prefix_and_suffix_mistakes() {
        use cellar_core::model::{ModuleEntry, SourceForm};
        let entry = |name: &str| ModuleEntry {
            name: name.to_string(),
            file: String::new(),
            deps: vec![],
            dependents: vec![],
            exports: vec![],
            functions: vec![],
            raw_sha256: String::new(),
            raw_len: 0,
            stored_len: 0,
            chunk: String::new(),
            form: SourceForm::Pretty,
            variants: vec![],
        };
        let index = ModuleIndex::new(
            BundleId::new(Platform::Whatsapp, 1),
            vec![entry("WAWebSendMsgStanza"), entry("WAWebReceipt")],
        );
        assert_eq!(suggest(&index, "SendMsg"), ["WAWebSendMsgStanza"]);
        assert_eq!(
            suggest(&index, "WAWebReceiptBatcherExtra"),
            ["WAWebReceipt"],
            "a name with one segment too many still finds its base"
        );
        assert!(suggest(&index, "zzz").is_empty());
    }

    #[test]
    fn is_inside_resolves_traversal_and_symlinks() {
        let base = std::env::temp_dir()
            .join(format!("cellar-inside-{}", std::process::id()))
            .join("bundles");
        let bundle = base.join("whatsapp-1");
        let outside = base.join("elsewhere");
        std::fs::create_dir_all(bundle.join(".chunks")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(is_inside(&bundle.join(".chunks"), &bundle));
        assert!(is_inside(&bundle, &bundle), "a directory is inside itself");
        assert!(!is_inside(&outside, &bundle));
        // `..` must not be able to present an inside path as an outside one.
        assert!(is_inside(
            &bundle.join(".chunks").join("..").join(".chunks"),
            &bundle
        ));
        // A path that does not exist is not inside anything.
        assert!(!is_inside(&bundle.join("nope"), &bundle));

        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    #[test]
    fn home_expansion_only_touches_a_leading_tilde() {
        unsafe { std::env::set_var("HOME", "/home/x") };
        assert_eq!(expand_home(Path::new("~/a")), PathBuf::from("/home/x/a"));
        assert_eq!(
            expand_home(Path::new("/abs/~/a")),
            PathBuf::from("/abs/~/a")
        );
    }
}
