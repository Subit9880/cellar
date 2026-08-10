//! Searching a bundle by module name and by source content.
//!
//! Both narrowings are available together and the cheap one runs first: a name
//! pattern selects candidates from the in-memory index, and only those files are
//! read for a content pattern. Searching `^WAWebSendMsg` for `addon` therefore
//! opens a few dozen files rather than 100k.
//!
//! Results always carry the module's path, because the expected next step is for
//! the caller to open the file with its own tools.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::filter::{FilterSet, Verdict};
use crate::model::ModuleIndex;
use crate::store::BundleHandle;

/// What to search for. An empty query (no name, no source) matches every module,
/// which is how "list what this filter keeps" is expressed.
#[derive(Debug, Default)]
pub struct Query {
    /// Regex over module names.
    pub name: Option<Regex>,
    /// Regex over module source text.
    pub source: Option<Regex>,
    /// Regex over export names.
    pub exports: Option<Regex>,
    /// Restrict to modules this filter keeps.
    pub filter: Option<FilterSet>,
    /// Maximum modules returned. Truncation is reported, never silent.
    pub limit: usize,
    /// Maximum matching lines reported per module.
    pub max_matches_per_module: usize,
    /// Lines of context around each source match.
    pub context_lines: usize,
}

impl Query {
    pub fn new() -> Self {
        Self {
            limit: 50,
            max_matches_per_module: 5,
            context_lines: 0,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineMatch {
    /// 1-based line number in the stored source file.
    pub line: u32,
    pub text: String,
    /// Lines immediately before `line`, when context was requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub name: String,
    /// Path relative to the bundle directory.
    pub file: String,
    /// Absolute path, ready to hand to a file-reading tool.
    pub path: String,
    pub dep_count: u32,
    pub dependent_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<String>,
    /// Matching source lines, when a source pattern was given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matches: Vec<LineMatch>,
    /// Total matching lines in this module, which can exceed `matches.len()`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub bundle: String,
    /// Modules considered after the filter and name narrowing.
    pub candidates: u64,
    /// Modules that matched everything asked for.
    pub matched: u64,
    /// True when `matched` exceeded the limit and `hits` was cut short.
    pub truncated: bool,
    /// Modules whose source could not be read. Reported so a shortfall in results
    /// is never mistaken for an absence of matches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    pub hits: Vec<SearchHit>,
}

/// Run a query against one bundle.
pub fn search(handle: &BundleHandle, index: &ModuleIndex, q: &Query) -> Result<SearchResult> {
    let decision = q.filter.as_ref().map(|f| f.apply(index));

    let candidates: Vec<&crate::model::ModuleEntry> = index
        .modules
        .iter()
        .filter(|m| decision.as_ref().is_none_or(|d| d.keeps(&m.name)))
        .filter(|m| q.name.as_ref().is_none_or(|re| re.is_match(&m.name)))
        .filter(|m| {
            q.exports
                .as_ref()
                .is_none_or(|re| m.exports.iter().any(|e| re.is_match(e)))
        })
        .collect();

    let mut unreadable = Vec::new();
    let mut hits: Vec<SearchHit> = match &q.source {
        // No content pattern: the name/export narrowing is the whole answer, and no
        // file has to be opened.
        None => candidates
            .iter()
            .map(|m| SearchHit {
                name: m.name.clone(),
                file: m.file.clone(),
                path: handle.module_path(m).display().to_string(),
                dep_count: m.deps.len() as u32,
                dependent_count: m.dependents.len() as u32,
                exports: m.exports.clone(),
                matches: Vec::new(),
                match_count: None,
            })
            .collect(),
        Some(re) => {
            // Reading and scanning is the expensive part and is embarrassingly
            // parallel; the index lookup above already cut the candidate set.
            let scanned: Vec<Result<Option<SearchHit>, (String, String)>> = candidates
                .par_iter()
                .map(|m| {
                    let path = handle.module_path(m);
                    let text = match std::fs::read_to_string(&path) {
                        Ok(t) => t,
                        Err(e) => return Err((m.name.clone(), e.to_string())),
                    };
                    Ok(scan_text(&text, re, q).map(|(matches, count)| SearchHit {
                        name: m.name.clone(),
                        file: m.file.clone(),
                        path: path.display().to_string(),
                        dep_count: m.deps.len() as u32,
                        dependent_count: m.dependents.len() as u32,
                        exports: m.exports.clone(),
                        matches,
                        match_count: Some(count),
                    }))
                })
                .collect();

            let mut out = Vec::new();
            for r in scanned {
                match r {
                    Ok(Some(hit)) => out.push(hit),
                    Ok(None) => {}
                    Err((name, err)) => unreadable.push(format!("{name}: {err}")),
                }
            }
            out
        }
    };

    // Deterministic order regardless of how the parallel scan interleaved.
    hits.sort_by(|a, b| a.name.cmp(&b.name));
    unreadable.sort();

    let matched = hits.len() as u64;
    let truncated = hits.len() > q.limit;
    hits.truncate(q.limit);

    Ok(SearchResult {
        bundle: index.bundle.to_string(),
        candidates: candidates.len() as u64,
        matched,
        truncated,
        unreadable,
        hits,
    })
}

/// Collect matching lines from one module's text. Returns `None` if nothing matched.
fn scan_text(text: &str, re: &Regex, q: &Query) -> Option<(Vec<LineMatch>, u32)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut matches = Vec::new();
    let mut count = 0u32;

    for (i, line) in lines.iter().enumerate() {
        if !re.is_match(line) {
            continue;
        }
        count += 1;
        if matches.len() >= q.max_matches_per_module {
            continue;
        }
        let before = if q.context_lines > 0 {
            lines[i.saturating_sub(q.context_lines)..i]
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            Vec::new()
        };
        let after = if q.context_lines > 0 {
            let end = (i + 1 + q.context_lines).min(lines.len());
            lines[i + 1..end].iter().map(|s| s.to_string()).collect()
        } else {
            Vec::new()
        };
        matches.push(LineMatch {
            line: (i + 1) as u32,
            text: line.to_string(),
            before,
            after,
        });
    }

    (count > 0).then_some((matches, count))
}

/// Read a slice of one module's source, for viewing.
///
/// `start` is 1-based and inclusive; `count` of `None` reads to the end.
pub fn read_lines(path: &PathBuf, start: u32, count: Option<u32>) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if start <= 1 && count.is_none() {
        return Ok(text);
    }
    let skip = start.saturating_sub(1) as usize;
    let take = count.map_or(usize::MAX, |c| c as usize);
    let mut out = String::new();
    for line in text.lines().skip(skip).take(take) {
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// Whether a filter keeps a name, for callers that only have the name.
pub fn keeps(filter: &FilterSet, name: &str) -> bool {
    matches!(filter.classify(name).0, Verdict::Keep)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BundleId, Platform, SourceForm};
    use crate::store::Store;
    use std::fs;

    fn fixture() -> (Store, BundleHandle, ModuleIndex, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "cellar-search-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let store = Store::open(Some(root.clone())).unwrap();
        let id = BundleId::new(Platform::Whatsapp, 1);
        let dir = store.begin_bundle(id).unwrap();

        let sources = [
            (
                "WAWebSendMsgStanza",
                "function send() {\n  addonType = 1;\n}\n",
            ),
            ("WAWebReceipt", "function ack() {\n  return true;\n}\n"),
            ("CometButton", "function render() {\n  addonType = 2;\n}\n"),
        ];
        let mut entries = Vec::new();
        for (name, src) in sources {
            let file = format!("modules/{name}.js");
            fs::write(dir.join(&file), src).unwrap();
            entries.push(crate::model::ModuleEntry {
                name: name.to_string(),
                file,
                deps: vec![],
                dependents: vec![],
                exports: vec![format!("{name}Export")],
                functions: vec![],
                raw_sha256: String::new(),
                raw_len: src.len() as u64,
                stored_len: src.len() as u64,
                chunk: "c.js".into(),
                form: SourceForm::Pretty,
                variants: vec![],
            });
        }
        let index = ModuleIndex::new(id, entries);
        let manifest = crate::model::BundleManifest {
            schema_version: crate::model::SCHEMA_VERSION.into(),
            bundle: id,
            source_url: None,
            archive_sha256: None,
            archive_len: None,
            indexed_at: "2026-01-01T00:00:00Z".into(),
            cellar_version: "test".into(),
            codegen_version: "test".into(),
            source_form: SourceForm::Pretty,
            modules_indexed: index.len() as u64,
            modules_bytes: 0,
            diagnostics: Default::default(),
        };
        let handle = store.commit_bundle(id, &index, &manifest).unwrap();
        (store, handle, index, root)
    }

    #[test]
    fn name_only_search_reads_no_files() {
        let (_s, handle, index, root) = fixture();
        let q = Query {
            name: Some(Regex::new("^WAWeb").unwrap()),
            ..Query::new()
        };
        let r = search(&handle, &index, &q).unwrap();
        assert_eq!(r.matched, 2);
        assert!(r.hits.iter().all(|h| h.matches.is_empty()));
        assert!(r.hits[0].path.ends_with("WAWebReceipt.js"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn source_search_reports_line_numbers() {
        let (_s, handle, index, root) = fixture();
        let q = Query {
            source: Some(Regex::new("addonType").unwrap()),
            ..Query::new()
        };
        let r = search(&handle, &index, &q).unwrap();
        assert_eq!(r.matched, 2);
        let hit = r.hits.iter().find(|h| h.name == "CometButton").unwrap();
        assert_eq!(hit.matches[0].line, 2);
        assert_eq!(hit.match_count, Some(1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn name_narrowing_applies_before_the_content_scan() {
        let (_s, handle, index, root) = fixture();
        let q = Query {
            name: Some(Regex::new("^WAWeb").unwrap()),
            source: Some(Regex::new("addonType").unwrap()),
            ..Query::new()
        };
        let r = search(&handle, &index, &q).unwrap();
        assert_eq!(r.candidates, 2, "only WAWeb* files were opened");
        assert_eq!(r.matched, 1);
        assert_eq!(r.hits[0].name, "WAWebSendMsgStanza");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filter_restricts_the_candidate_set() {
        let (_s, handle, index, root) = fixture();
        let q = Query {
            source: Some(Regex::new("addonType").unwrap()),
            filter: Some(FilterSet::compile(crate::builtin::default()).unwrap()),
            ..Query::new()
        };
        let r = search(&handle, &index, &q).unwrap();
        assert!(
            r.hits.iter().all(|h| h.name != "CometButton"),
            "the default filter excludes Comet* modules"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn truncation_is_reported() {
        let (_s, handle, index, root) = fixture();
        let q = Query {
            limit: 1,
            ..Query::new()
        };
        let r = search(&handle, &index, &q).unwrap();
        assert_eq!(r.matched, 3, "the true count survives truncation");
        assert_eq!(r.hits.len(), 1);
        assert!(r.truncated);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn context_lines_are_returned_when_asked_for() {
        let (_s, handle, index, root) = fixture();
        let q = Query {
            source: Some(Regex::new("addonType").unwrap()),
            context_lines: 1,
            ..Query::new()
        };
        let r = search(&handle, &index, &q).unwrap();
        let hit = &r.hits[0];
        assert_eq!(hit.matches[0].before.len(), 1);
        assert!(hit.matches[0].before[0].contains("function"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_lines_slices_by_1_based_line_number() {
        let (_s, handle, index, root) = fixture();
        let entry = index.get("WAWebSendMsgStanza").unwrap();
        let path = handle.module_path(entry);
        assert_eq!(read_lines(&path, 2, Some(1)).unwrap(), "  addonType = 1;\n");
        assert!(read_lines(&path, 1, None).unwrap().starts_with("function"));
        let _ = fs::remove_dir_all(&root);
    }
}
