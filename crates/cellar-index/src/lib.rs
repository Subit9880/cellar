//! Turning a directory of bundle chunks into an indexed cellar bundle.
//!
//! # Why an AST and not a regex
//!
//! A chunk is one line of minified JavaScript holding many
//! `__d("Name", [deps], factory, id)` definitions back to back. Finding where one
//! ends means balancing parentheses through string literals, template literals,
//! regex literals and comments — and getting it wrong truncates a module silently,
//! which then shows up as a spurious diff on the next revision. oxc answers the
//! question exactly, and its byte spans give the module slice for free.
//!
//! # Determinism
//!
//! Chunks are parsed in parallel, which means they finish in an arbitrary order,
//! but nothing observable may depend on that order — filename assignment in
//! particular, since two modules can compete for one filename (see
//! [`cellar_core::naming`]). So parsing writes each module's source to a staging
//! name derived from the module name alone, and a second, serial pass walks the
//! modules in sorted order and renames each staged file into place. The parallel
//! phase does the expensive work; the serial phase does the order-sensitive work,
//! on metadata only.

pub mod extract;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cellar_core::model::{
    BundleId, Diagnostics, ModuleEntry, ModuleIndex, ModuleVariant, SourceForm,
};
use cellar_core::naming::FileNamer;
use oxc_allocator::Allocator;
use oxc_ast::ast::{CallExpression, Expression};
use oxc_ast_visit::{Visit, walk};
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use rayon::prelude::*;
use sha2::{Digest, Sha256};

/// Version tag recorded in the manifest, so a bundle indexed by an older printer
/// is explainable rather than mysteriously different.
pub const CODEGEN_VERSION: &str = concat!("oxc_codegen ", env!("CARGO_PKG_VERSION"), "/0.143");

/// Directory the parallel phase stages module sources into, under `modules/`.
const STAGE: &str = ".stage";

/// Cap on the sample of unresolved dependency names kept in the diagnostics.
const UNRESOLVED_SAMPLE: usize = 50;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Re-print module sources from the AST. Off stores the bytes as served, which
    /// is exact but leaves every module on one enormous line.
    pub pretty: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self { pretty: true }
    }
}

/// What indexing produced.
#[derive(Debug)]
pub struct IndexOutcome {
    pub index: ModuleIndex,
    pub diagnostics: Diagnostics,
    pub modules_bytes: u64,
    pub source_form: SourceForm,
}

/// One module as found by the parallel phase, before filenames are assigned.
struct Staged {
    facts: extract::ModuleFacts,
    raw_sha256: String,
    raw_len: u64,
    stored_len: u64,
    chunk: String,
    form: SourceForm,
    stage_file: PathBuf,
}

/// Index every chunk in `chunks_dir`, writing module sources under
/// `bundle_dir/modules/`.
pub fn index_directory(
    chunks_dir: &Path,
    bundle_dir: &Path,
    bundle: BundleId,
    opts: &IndexOptions,
) -> Result<IndexOutcome> {
    let modules_dir = bundle_dir.join("modules");
    let stage_dir = modules_dir.join(STAGE);
    fs::create_dir_all(&stage_dir).with_context(|| format!("creating {}", stage_dir.display()))?;

    let chunks = collect_chunks(chunks_dir)?;
    if chunks.is_empty() {
        bail!(
            "no files found under {} — is this an extracted bundle archive?",
            chunks_dir.display()
        );
    }

    let mut diagnostics = Diagnostics::default();
    let js: Vec<PathBuf> = chunks
        .into_iter()
        .filter(|p| {
            let is_js = p.extension().and_then(|e| e.to_str()) == Some("js");
            if !is_js {
                diagnostics.chunks_skipped += 1;
            }
            is_js
        })
        .collect();
    diagnostics.chunks_read = js.len() as u64;

    // --- parallel phase: parse, print, stage -------------------------------
    let results: Vec<ChunkOutcome> = js
        .par_iter()
        .map(|path| index_chunk(path, &stage_dir, opts))
        .collect();

    // Every textually distinct definition of a name is kept. A quarter of the
    // modules in a real bundle ship in more than one form, and collapsing them to
    // whichever the walk saw first both loses information and makes the choice
    // depend on chunk naming.
    let mut staged: BTreeMap<String, Vec<Staged>> = BTreeMap::new();
    let mut divergent: BTreeMap<String, u64> = BTreeMap::new();

    for outcome in results {
        match outcome {
            ChunkOutcome::Failed { path, error } => {
                diagnostics.chunk_parse_failures += 1;
                // Keep going: one unparseable chunk should cost its own modules,
                // not the whole bundle. The counter is what makes it visible.
                let _ = (path, error);
            }
            ChunkOutcome::Parsed {
                modules,
                codegen_failures,
            } => {
                diagnostics.codegen_failures += codegen_failures;
                for m in modules {
                    let copies = staged.entry(m.facts.name.clone()).or_default();
                    // A byte-identical re-definition is ordinary Metro output and
                    // carries no information; only distinct bodies are variants.
                    if copies.iter().any(|c| c.raw_sha256 == m.raw_sha256) {
                        continue;
                    }
                    if !copies.is_empty() {
                        *divergent.entry(m.facts.name.clone()).or_insert(0) += 1;
                    }
                    copies.push(m);
                }
            }
        }
    }

    if staged.is_empty() {
        bail!(
            "indexed 0 modules from {} chunks in {} — the bundle format may have changed \
             (expected `__d(\"Name\", [deps], factory)` definitions)",
            diagnostics.chunks_read,
            chunks_dir.display()
        );
    }

    // --- serial phase: assign filenames, rename into place -----------------
    let mut namer = FileNamer::new();
    let mut entries = Vec::with_capacity(staged.len());
    let mut modules_bytes = 0u64;

    // `staged` is a BTreeMap, so this walks module names in sorted order.
    for (name, mut copies) in staged {
        // Pick the primary by content, never by where it was found. Ordering on
        // (length desc, hash asc) means the same set of definitions always yields
        // the same primary, so a diff between two revisions reports a change only
        // when a definition really changed — not when the walk happened to reach a
        // different chunk first. The longest is chosen because a variant that adds
        // a dependency or a branch is a superset of the one that does not, and the
        // richer text is the more informative one to read and to diff.
        copies.sort_by(|a, b| {
            b.raw_len
                .cmp(&a.raw_len)
                .then_with(|| a.raw_sha256.cmp(&b.raw_sha256))
        });
        let primary = copies.remove(0);

        let file_name = namer.file_for(&name);
        let rel = format!("modules/{file_name}");
        let stem = file_name
            .strip_suffix(".js")
            .unwrap_or(&file_name)
            .to_string();
        move_into_place(&primary.stage_file, &bundle_dir.join(&rel))?;
        modules_bytes += primary.stored_len;

        let mut variants = Vec::with_capacity(copies.len());
        for copy in copies {
            // `~` cannot occur in a primary stem (it is not a filename-safe
            // character, and the disambiguating suffix the namer appends is
            // `~<hex>`), so `~alt-` can never collide with a real module's file.
            let alt_rel = format!("modules/{stem}~alt-{}.js", &copy.raw_sha256[..16]);
            move_into_place(&copy.stage_file, &bundle_dir.join(&alt_rel))?;
            modules_bytes += copy.stored_len;
            variants.push(ModuleVariant {
                sha256: copy.raw_sha256,
                file: alt_rel,
                raw_len: copy.raw_len,
                deps_added: missing_from(&copy.facts.deps, &primary.facts.deps),
                deps_removed: missing_from(&primary.facts.deps, &copy.facts.deps),
            });
        }

        entries.push(ModuleEntry {
            name,
            file: rel,
            deps: primary.facts.deps,
            dependents: Vec::new(),
            exports: primary.facts.exports,
            functions: primary.facts.functions,
            raw_sha256: primary.raw_sha256,
            raw_len: primary.raw_len,
            stored_len: primary.stored_len,
            chunk: primary.chunk,
            form: primary.form,
            variants,
        });
    }

    fs::remove_dir_all(&stage_dir).ok();
    diagnostics.renamed_files = namer.renamed();
    diagnostics.modules_with_variants = divergent;

    let mut index = ModuleIndex::new(bundle, entries);
    let unresolved = link_dependents(&mut index);
    diagnostics.unresolved_deps_total = unresolved.len() as u64;
    diagnostics.unresolved_deps = unresolved.into_iter().take(UNRESOLVED_SAMPLE).collect();

    Ok(IndexOutcome {
        modules_bytes,
        source_form: if opts.pretty {
            SourceForm::Pretty
        } else {
            SourceForm::Raw
        },
        diagnostics,
        index,
    })
}

/// Move a staged file to its final path.
///
/// Deliberately not tolerant of a missing source: staged paths are unique per
/// (module name, content) and byte-identical definitions are collapsed before this
/// runs, so a missing file means an invariant broke. Skipping quietly would leave
/// an index entry pointing at a path that does not exist — the exact failure that
/// looks fine until someone tries to read it.
fn move_into_place(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).with_context(|| {
        format!(
            "moving staged source into place: {} -> {}",
            from.display(),
            to.display()
        )
    })
}

/// Entries of `from` absent from `to`, sorted.
fn missing_from(from: &[String], to: &[String]) -> Vec<String> {
    let to: std::collections::BTreeSet<&str> = to.iter().map(String::as_str).collect();
    let mut out: Vec<String> = from
        .iter()
        .filter(|d| !to.contains(d.as_str()))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Fill in every module's `dependents`, returning the sorted set of dependency
/// names no module in this bundle defines.
fn link_dependents(index: &mut ModuleIndex) -> Vec<String> {
    let edges: Vec<(String, String)> = index
        .modules
        .iter()
        .flat_map(|m| m.deps.iter().map(|d| (d.clone(), m.name.clone())))
        .collect();

    let mut unresolved: Vec<String> = Vec::new();
    for (dep, dependent) in edges {
        match index.modules.binary_search_by(|m| m.name.cmp(&dep)) {
            Ok(i) => index.modules[i].dependents.push(dependent),
            Err(_) => unresolved.push(dep),
        }
    }
    for m in &mut index.modules {
        m.dependents.sort();
        m.dependents.dedup();
    }
    unresolved.sort();
    unresolved.dedup();
    unresolved
}

enum ChunkOutcome {
    Parsed {
        modules: Vec<Staged>,
        codegen_failures: u64,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

fn index_chunk(path: &Path, stage_dir: &Path, opts: &IndexOptions) -> ChunkOutcome {
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            return ChunkOutcome::Failed {
                path: path.to_path_buf(),
                error: e.to_string(),
            };
        }
    };

    let allocator = Allocator::default();
    let ret = Parser::new(&allocator, &source, SourceType::cjs()).parse();
    if ret.panicked {
        return ChunkOutcome::Failed {
            path: path.to_path_buf(),
            error: "parser panicked".into(),
        };
    }

    let mut spans = DefineSpans::default();
    spans.visit_program(&ret.program);

    let chunk_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();

    let mut modules = Vec::with_capacity(spans.spans.len());
    let mut codegen_failures = 0u64;

    for (start, end) in spans.spans {
        let raw = &source[start..end];
        let raw_sha256 = hex::encode(Sha256::digest(raw.as_bytes()));

        // Re-parse the isolated slice: it is what gets printed, and it is what the
        // facts must describe, so both come from the same tree.
        let alloc = Allocator::default();
        let parsed = Parser::new(&alloc, raw, SourceType::cjs()).parse();
        let Some(facts) = extract::facts_of_module(&parsed.program) else {
            continue;
        };

        let (text, form) = if opts.pretty && !parsed.panicked {
            let printed = Codegen::new()
                .with_options(CodegenOptions::default())
                .build(&parsed.program)
                .code;
            if printed.trim().is_empty() {
                codegen_failures += 1;
                (raw.to_string(), SourceForm::Raw)
            } else {
                (printed, SourceForm::Pretty)
            }
        } else {
            if opts.pretty {
                codegen_failures += 1;
            }
            (raw.to_string(), SourceForm::Raw)
        };

        // Staged under (module name, content), never under the name alone: two
        // chunks can define the same module with different bodies, and a
        // name-only staging path would have the second write clobber the first.
        // Keying on content too means byte-identical copies share one file
        // (harmless, it is the same bytes) while divergent copies stay separate.
        // Either way the path does not depend on which thread arrived first.
        let stage_file = stage_dir.join(format!(
            "{}-{}.js",
            hex::encode(Sha256::digest(facts.name.as_bytes())),
            &raw_sha256[..16]
        ));
        if fs::write(&stage_file, text.as_bytes()).is_err() {
            continue;
        }

        modules.push(Staged {
            raw_sha256,
            raw_len: raw.len() as u64,
            stored_len: text.len() as u64,
            chunk: chunk_name.clone(),
            form,
            stage_file,
            facts,
        });
    }

    ChunkOutcome::Parsed {
        modules,
        codegen_failures,
    }
}

/// Byte spans of every top-level `__d(...)` call in a chunk.
#[derive(Default)]
struct DefineSpans {
    spans: Vec<(usize, usize)>,
}

impl<'a> Visit<'a> for DefineSpans {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let is_define = matches!(&call.callee, Expression::Identifier(id)
            if id.name.as_str() == extract::DEFINE_FN)
            && call.arguments.len() >= 3;
        if is_define {
            let span = call.span();
            self.spans.push((span.start as usize, span.end as usize));
            // `__d` is never nested inside another factory, so not descending here
            // skips traversing every module body a second time.
            return;
        }
        walk::walk_call_expression(self, call);
    }
}

/// Every regular file under `dir`, sorted, so the merge order is fixed.
fn collect_chunks(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() {
                out.push(path);
            } else if ty.is_symlink() {
                // `file_type()` describes the link itself, not its target, so a
                // symlinked chunk reads as neither file nor directory here. They
                // must be followed: importing an existing extracted bundle links
                // its chunks in rather than copying ~1.5 GB to read them once.
                // Only regular-file targets are taken, and a symlinked directory is
                // never descended into, so a link cycle cannot make this loop.
                if fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cellar_core::model::Platform;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cellar-index-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build a chunk directory from `(filename, source)` pairs and index it.
    fn index_fixture(chunks: &[(&str, &str)]) -> (IndexOutcome, PathBuf) {
        let base = temp_dir("fixture");
        let chunks_dir = base.join("chunks");
        let bundle_dir = base.join("bundle");
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(bundle_dir.join("modules")).unwrap();
        for (name, src) in chunks {
            fs::write(chunks_dir.join(name), src).unwrap();
        }
        let outcome = index_directory(
            &chunks_dir,
            &bundle_dir,
            BundleId::new(Platform::Whatsapp, 1),
            &IndexOptions::default(),
        )
        .unwrap();
        (outcome, base)
    }

    #[test]
    fn extracts_modules_and_links_the_reverse_edges() {
        let (out, base) = index_fixture(&[(
            "a.js",
            r#"__d("Core",[],function(g,r,i,a,m,e,d){e.go=1;},1);
               __d("User",["Core"],function(g,r,i,a,m,e,d){},2);"#,
        )]);
        assert_eq!(out.index.len(), 2);
        let core = out.index.get("Core").unwrap();
        assert_eq!(core.dependents, ["User"]);
        assert_eq!(core.exports, ["go"]);
        let user = out.index.get("User").unwrap();
        assert_eq!(user.deps, ["Core"]);
        assert!(user.dependents.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn module_sources_are_written_under_their_own_names() {
        let (out, base) = index_fixture(&[(
            "a.js",
            r#"__d("WAWebSendMsgStanza",[],function(g,r,i,a,m,e,d){e.send=function(){};},1);"#,
        )]);
        let entry = out.index.get("WAWebSendMsgStanza").unwrap();
        assert_eq!(entry.file, "modules/WAWebSendMsgStanza.js");
        let text = fs::read_to_string(base.join("bundle").join(&entry.file)).unwrap();
        assert!(text.contains("WAWebSendMsgStanza"));
        // Pretty-printed: the point is that it is no longer one line.
        assert!(text.lines().count() > 1, "got:\n{text}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn brace_and_string_hostile_sources_are_sliced_correctly() {
        // A `__d(` inside a string literal, and unbalanced-looking parens inside a
        // regex literal — both of which defeat brace counting.
        let (out, base) = index_fixture(&[(
            "a.js",
            r#"__d("Tricky",[],function(g,r,i,a,m,e,d){
                 var s = "__d(\"NotAModule\",[],function(){})";
                 var re = /\)\(/g;
                 e.value = s + re.source;
               },1);
               __d("After",[],function(g,r,i,a,m,e,d){},2);"#,
        )]);
        let names: Vec<&str> = out.index.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["After", "Tricky"],
            "the string-literal `__d(` must not be mistaken for a module"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn unresolved_dependencies_are_reported_not_dropped() {
        let (out, base) = index_fixture(&[(
            "a.js",
            r#"__d("Solo",["LivesInAnotherBundle"],function(g,r,i,a,m,e,d){},1);"#,
        )]);
        assert_eq!(out.diagnostics.unresolved_deps, ["LivesInAnotherBundle"]);
        assert_eq!(out.diagnostics.unresolved_deps_total, 1);
        // The edge stays on the module itself, so the graph can still show it.
        assert_eq!(
            out.index.get("Solo").unwrap().deps,
            ["LivesInAnotherBundle"]
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn identical_duplicates_are_quiet() {
        let same = r#"__d("Dup",[],function(g,r,i,a,m,e,d){e.v=1;},1);"#;
        let (out, base) = index_fixture(&[("a.js", same), ("b.js", same)]);
        assert_eq!(out.index.len(), 1);
        assert!(
            out.diagnostics.modules_with_variants.is_empty(),
            "a byte-identical copy is normal Metro output and says nothing"
        );
        assert!(!out.index.get("Dup").unwrap().has_variants());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn divergent_definitions_are_all_kept_and_written_out() {
        // Around a quarter of real bundle modules ship in more than one form. The
        // extra ones are written beside the primary rather than discarded.
        let (out, base) = index_fixture(&[
            (
                "a.js",
                r#"__d("Dup",["Base"],function(g,r,i,a,m,e,d){e.v=1;},1);"#,
            ),
            (
                "b.js",
                r#"__d("Dup",["Base","Extra"],function(g,r,i,a,m,e,d){e.v=1;e.w=2;},1);"#,
            ),
        ]);
        assert_eq!(out.index.len(), 1);
        assert_eq!(out.diagnostics.modules_with_variants.get("Dup"), Some(&1));

        let entry = out.index.get("Dup").unwrap();
        assert!(entry.has_variants());
        // The longer definition became primary, so `deps` shows the richer set.
        assert_eq!(entry.deps, ["Base", "Extra"]);
        let variant = &entry.variants[0];
        assert_eq!(variant.deps_removed, ["Extra"]);
        assert!(variant.deps_added.is_empty());

        // Both definitions exist on disk and are readable.
        let bundle = base.join("bundle");
        assert!(bundle.join(&entry.file).is_file());
        assert!(bundle.join(&variant.file).is_file(), "{}", variant.file);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn the_primary_does_not_depend_on_which_chunk_was_seen_first() {
        // The same two definitions in either chunk order must produce the same
        // primary — otherwise a diff between revisions reports a change whenever
        // chunk naming shifts, which it does on every build.
        let short = r#"__d("Dup",[],function(g,r,i,a,m,e,d){e.v=1;},1);"#;
        let long = r#"__d("Dup",[],function(g,r,i,a,m,e,d){e.v=1;e.extra=2;},1);"#;

        let (first, base1) = index_fixture(&[("a.js", short), ("b.js", long)]);
        let (second, base2) = index_fixture(&[("a.js", long), ("b.js", short)]);
        assert_eq!(
            first.index.get("Dup").unwrap().raw_sha256,
            second.index.get("Dup").unwrap().raw_sha256
        );
        assert_eq!(
            first.index.get("Dup").unwrap().fingerprint(),
            second.index.get("Dup").unwrap().fingerprint()
        );
        let _ = fs::remove_dir_all(&base1);
        let _ = fs::remove_dir_all(&base2);
    }

    #[test]
    fn an_unparseable_chunk_costs_only_its_own_modules() {
        let (out, base) = index_fixture(&[
            ("a.js", r#"__d("Good",[],function(g,r,i,a,m,e,d){},1);"#),
            ("b.js", "function ( { this is not javascript"),
        ]);
        assert!(out.index.contains("Good"));
        assert_eq!(out.diagnostics.chunk_parse_failures, 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_chunks_are_followed() {
        // This is how `bundle import` presents an existing extracted bundle: a
        // directory of links, not a 1.5 GB copy. `read_dir` reports a link as
        // neither file nor directory, so the walker has to resolve it explicitly.
        let base = temp_dir("symlink");
        let real = base.join("real");
        let chunks_dir = base.join("chunks");
        let bundle_dir = base.join("bundle");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(bundle_dir.join("modules")).unwrap();

        let target = real.join("a.js");
        fs::write(&target, r#"__d("Linked",[],function(g,r,i,a,m,e,d){},1);"#).unwrap();
        std::os::unix::fs::symlink(&target, chunks_dir.join("a.js")).unwrap();

        let out = index_directory(
            &chunks_dir,
            &bundle_dir,
            BundleId::new(Platform::Whatsapp, 1),
            &IndexOptions::default(),
        )
        .unwrap();
        assert!(out.index.contains("Linked"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn non_js_members_are_counted_as_skipped() {
        let (out, base) = index_fixture(&[
            ("a.js", r#"__d("Good",[],function(g,r,i,a,m,e,d){},1);"#),
            ("style.css", ".a{color:red}"),
        ]);
        assert_eq!(out.diagnostics.chunks_read, 1);
        assert_eq!(out.diagnostics.chunks_skipped, 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn indexing_is_deterministic_across_runs() {
        let chunks: Vec<(&str, &str)> = vec![
            (
                "a.js",
                r#"__d("Beta",["Alpha"],function(g,r,i,a,m,e,d){},1);"#,
            ),
            (
                "b.js",
                r#"__d("Alpha",[],function(g,r,i,a,m,e,d){e.x=1;},2);"#,
            ),
            (
                "c.js",
                r#"__d("Gamma",["Beta","Alpha"],function(g,r,i,a,m,e,d){},3);"#,
            ),
        ];
        let (first, base1) = index_fixture(&chunks);
        let (second, base2) = index_fixture(&chunks);
        assert_eq!(first.index, second.index);
        let _ = fs::remove_dir_all(&base1);
        let _ = fs::remove_dir_all(&base2);
    }

    #[test]
    fn an_archive_with_no_modules_fails_loudly() {
        let base = temp_dir("empty");
        let chunks_dir = base.join("chunks");
        let bundle_dir = base.join("bundle");
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(bundle_dir.join("modules")).unwrap();
        fs::write(chunks_dir.join("a.js"), "var x = 1;").unwrap();

        let err = index_directory(
            &chunks_dir,
            &bundle_dir,
            BundleId::new(Platform::Whatsapp, 1),
            &IndexOptions::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("indexed 0 modules"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }
}
