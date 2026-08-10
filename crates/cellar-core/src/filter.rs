//! Named, user-editable filters that decide which modules an operation looks at.
//!
//! A bundle holds ~100k modules, the overwhelming majority of which are React
//! components, icons, ad-platform code and analytics for the other Meta products
//! that share the bundle. A raw revision-to-revision diff is therefore tens of
//! thousands of changes and useless. A filter cuts that to the protocol surface.
//!
//! # Precedence
//!
//! Three name-pattern lists, checked in this order — the first match wins:
//!
//! 1. [`Filter::hard_exclude`] — dropped unconditionally. This exists for module
//!    classes that are *handled by a different tool* rather than uninteresting:
//!    `.pb` protobuf schemas and `.graphql` documents are structured artifacts, and
//!    a text diff of one is noise even though the module is highly protocol-relevant.
//! 2. [`Filter::include`] — kept, overriding `exclude`. A module named
//!    `WAWebSendMsgStanzaAction` matches the generic `Action$` exclusion but is
//!    exactly what we care about.
//! 3. [`Filter::exclude`] — dropped.
//!
//! Anything matching none of them gets [`Filter::default_verdict`], which is what
//! separates a deny-list filter (`default`: keep unless excluded) from an
//! allow-list one (`protocol`: drop unless included).
//!
//! # Dependency propagation
//!
//! With [`Filter::exclude_dependents_of_excluded`], a module that depends on an
//! excluded module is itself excluded — the transitive closure of "touches UI".
//! This runs as a worklist to a fixpoint, so the result depends only on the graph
//! and not on the order modules were visited. (ProtoCocktail's equivalent
//! accumulated a skiplist while walking files, which made the outcome depend on
//! directory iteration order.) Modules matched by `include` are exempt: they are
//! wanted regardless of what they pull in.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Context, Result};
use regex::RegexSet;
use serde::{Deserialize, Serialize};

use crate::model::ModuleIndex;

/// Whether a module is kept or dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Keep,
    Drop,
}

/// A named filter, as stored in `filters/<name>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Filter {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Dropped before anything else is considered. See the module docs.
    #[serde(default)]
    pub hard_exclude: Vec<String>,
    /// Kept, overriding [`Filter::exclude`].
    #[serde(default)]
    pub include: Vec<String>,
    /// Dropped unless `include` matched first.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Verdict for a module no list matched.
    pub default_verdict: Verdict,
    /// Drop modules that (transitively) depend on a dropped module.
    #[serde(default)]
    pub exclude_dependents_of_excluded: bool,
    /// Dependency names ignored when reporting a module's added/removed deps.
    /// These churn for reasons unrelated to the protocol (a build-tooling change
    /// swapping a runtime helper) and would otherwise flag every module.
    #[serde(default)]
    pub noise_deps: Vec<String>,
    /// Source lines matching these are ignored when deciding whether a module's
    /// change is substantive. A module whose entire diff is transpiler-helper
    /// churn is reported as `noiseOnly` rather than as a real change.
    #[serde(default)]
    pub noise_code: Vec<String>,
    /// True for the compiled-in filters, which cannot be edited or deleted in
    /// place. `cellar filter fork <builtin> <new>` makes an editable copy.
    #[serde(default, skip_serializing_if = "is_false")]
    pub builtin: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Why one module got its verdict. Carried through so a filter can be explained
/// and debugged, rather than being an opaque yes/no.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase")]
pub enum Because {
    /// Matched a `hardExclude` pattern.
    HardExclude { pattern: String },
    /// Matched an `include` pattern.
    Include { pattern: String },
    /// Matched an `exclude` pattern.
    Exclude { pattern: String },
    /// Matched nothing; took `defaultVerdict`.
    Default,
    /// Dropped because it depends on a dropped module.
    DependsOn { module: String },
}

/// A compiled filter, ready to apply.
///
/// Compiling turns each pattern list into one [`RegexSet`], so classifying 100k
/// module names is a handful of single-pass automaton runs rather than 100k × 200
/// individual regex matches.
#[derive(Debug)]
pub struct FilterSet {
    filter: Filter,
    hard_exclude: RegexSet,
    include: RegexSet,
    exclude: RegexSet,
    noise_deps: RegexSet,
    noise_code: RegexSet,
}

fn compile(patterns: &[String], field: &str, filter: &str) -> Result<RegexSet> {
    RegexSet::new(patterns)
        .with_context(|| format!("filter {filter:?} has an invalid pattern in `{field}`"))
}

impl FilterSet {
    pub fn compile(filter: Filter) -> Result<Self> {
        let n = &filter.name;
        Ok(Self {
            hard_exclude: compile(&filter.hard_exclude, "hardExclude", n)?,
            include: compile(&filter.include, "include", n)?,
            exclude: compile(&filter.exclude, "exclude", n)?,
            noise_deps: compile(&filter.noise_deps, "noiseDeps", n)?,
            noise_code: compile(&filter.noise_code, "noiseCode", n)?,
            filter,
        })
    }

    pub fn filter(&self) -> &Filter {
        &self.filter
    }

    pub fn name(&self) -> &str {
        &self.filter.name
    }

    /// Classify one module name, ignoring dependency propagation.
    pub fn classify(&self, name: &str) -> (Verdict, Because) {
        if let Some(i) = self.hard_exclude.matches(name).iter().next() {
            return (
                Verdict::Drop,
                Because::HardExclude {
                    pattern: self.filter.hard_exclude[i].clone(),
                },
            );
        }
        if let Some(i) = self.include.matches(name).iter().next() {
            return (
                Verdict::Keep,
                Because::Include {
                    pattern: self.filter.include[i].clone(),
                },
            );
        }
        if let Some(i) = self.exclude.matches(name).iter().next() {
            return (
                Verdict::Drop,
                Because::Exclude {
                    pattern: self.filter.exclude[i].clone(),
                },
            );
        }
        (self.filter.default_verdict, Because::Default)
    }

    /// Whether a dependency name should be ignored when reporting dep changes.
    pub fn is_noise_dep(&self, dep: &str) -> bool {
        self.noise_deps.is_match(dep)
    }

    /// Whether a source line is transpiler/bundler churn rather than real change.
    pub fn is_noise_line(&self, line: &str) -> bool {
        self.noise_code.is_match(line)
    }

    /// Apply the filter to a whole index, including dependency propagation.
    pub fn apply(&self, index: &ModuleIndex) -> FilterDecision {
        let mut verdict: BTreeMap<&str, (Verdict, Because)> = BTreeMap::new();
        // Modules matched by `include` are pinned: dependency propagation must not
        // take them back out.
        let mut pinned: BTreeSet<&str> = BTreeSet::new();

        for m in &index.modules {
            let (v, why) = self.classify(&m.name);
            if matches!(why, Because::Include { .. }) {
                pinned.insert(m.name.as_str());
            }
            verdict.insert(m.name.as_str(), (v, why));
        }

        if self.filter.exclude_dependents_of_excluded {
            // Worklist to a fixpoint: seed with everything already dropped, then
            // push each newly dropped module's dependents. Order-independent, and
            // each module can enter the queue at most once (it is only pushed on
            // the transition Keep -> Drop).
            let mut queue: VecDeque<&str> = verdict
                .iter()
                .filter(|(_, (v, _))| *v == Verdict::Drop)
                .map(|(name, _)| *name)
                .collect();

            while let Some(dropped) = queue.pop_front() {
                let Some(entry) = index.get(dropped) else {
                    continue;
                };
                for dependent in &entry.dependents {
                    let dep_name = dependent.as_str();
                    if pinned.contains(dep_name) {
                        continue;
                    }
                    let Some(slot) = verdict.get_mut(dep_name) else {
                        continue;
                    };
                    if slot.0 == Verdict::Keep {
                        *slot = (
                            Verdict::Drop,
                            Because::DependsOn {
                                module: dropped.to_string(),
                            },
                        );
                        // `dependents` is interned in the index, so re-borrowing by
                        // name keeps the 'index lifetime rather than the map's.
                        if let Some(e) = index.get(dep_name) {
                            queue.push_back(e.name.as_str());
                        }
                    }
                }
            }
        }

        let mut kept = BTreeSet::new();
        let mut reasons = BTreeMap::new();
        for (name, (v, why)) in verdict {
            if v == Verdict::Keep {
                kept.insert(name.to_string());
            }
            reasons.insert(name.to_string(), (v, why));
        }

        FilterDecision {
            filter: self.filter.name.clone(),
            total: index.len(),
            kept,
            reasons,
        }
    }
}

/// The outcome of applying a filter to one bundle.
#[derive(Debug, Clone)]
pub struct FilterDecision {
    pub filter: String,
    /// How many modules the index held before filtering.
    pub total: usize,
    /// Names that survived, sorted.
    pub kept: BTreeSet<String>,
    /// Per-module verdict and justification, for explaining the filter.
    pub reasons: BTreeMap<String, (Verdict, Because)>,
}

impl FilterDecision {
    pub fn keeps(&self, name: &str) -> bool {
        self.kept.contains(name)
    }

    pub fn kept_count(&self) -> usize {
        self.kept.len()
    }

    pub fn dropped_count(&self) -> usize {
        self.total.saturating_sub(self.kept.len())
    }

    /// Count of dropped modules by the kind of rule that dropped them.
    pub fn drop_breakdown(&self) -> BTreeMap<&'static str, usize> {
        let mut out = BTreeMap::new();
        for (v, why) in self.reasons.values() {
            if *v != Verdict::Drop {
                continue;
            }
            let key = match why {
                Because::HardExclude { .. } => "hardExclude",
                Because::Exclude { .. } => "exclude",
                Because::DependsOn { .. } => "dependsOnExcluded",
                Because::Default => "default",
                Because::Include { .. } => continue,
            };
            *out.entry(key).or_insert(0) += 1;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BundleId, ModuleEntry, Platform, SourceForm};

    fn entry(name: &str, deps: &[&str]) -> ModuleEntry {
        ModuleEntry {
            name: name.to_string(),
            file: format!("modules/{name}.js"),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            dependents: vec![],
            exports: vec![],
            functions: vec![],
            raw_sha256: String::new(),
            raw_len: 0,
            stored_len: 0,
            chunk: String::new(),
            form: SourceForm::Pretty,
            variants: vec![],
        }
    }

    /// Build an index and fill in reverse edges the way the indexer does.
    fn index(entries: Vec<ModuleEntry>) -> ModuleIndex {
        let mut idx = ModuleIndex::new(BundleId::new(Platform::Whatsapp, 1), entries);
        let edges: Vec<(String, String)> = idx
            .modules
            .iter()
            .flat_map(|m| m.deps.iter().map(|d| (d.clone(), m.name.clone())))
            .collect();
        for (dep, dependent) in edges {
            if let Ok(i) = idx.modules.binary_search_by(|m| m.name.cmp(&dep)) {
                idx.modules[i].dependents.push(dependent);
            }
        }
        for m in &mut idx.modules {
            m.dependents.sort();
            m.dependents.dedup();
        }
        idx
    }

    fn filter(f: Filter) -> FilterSet {
        FilterSet::compile(f).expect("compiles")
    }

    fn base() -> Filter {
        Filter {
            name: "t".into(),
            description: String::new(),
            hard_exclude: vec![],
            include: vec![],
            exclude: vec![],
            default_verdict: Verdict::Keep,
            exclude_dependents_of_excluded: false,
            noise_deps: vec![],
            noise_code: vec![],
            builtin: false,
        }
    }

    #[test]
    fn include_beats_exclude_but_not_hard_exclude() {
        let f = filter(Filter {
            hard_exclude: vec![r"\.pb$".into()],
            include: vec![r"^WAWeb".into()],
            exclude: vec![r"Action$".into()],
            ..base()
        });
        assert_eq!(f.classify("WAWebSendAction").0, Verdict::Keep);
        assert_eq!(f.classify("RandomAction").0, Verdict::Drop);
        // hardExclude wins even over an include match.
        assert_eq!(f.classify("WAWebFoo.pb").0, Verdict::Drop);
    }

    #[test]
    fn default_verdict_switches_deny_list_to_allow_list() {
        let allow = filter(Filter {
            include: vec![r"^WASignal".into()],
            default_verdict: Verdict::Drop,
            ..base()
        });
        assert_eq!(allow.classify("WASignalCipher").0, Verdict::Keep);
        assert_eq!(allow.classify("SomethingElse").0, Verdict::Drop);
    }

    #[test]
    fn dependency_propagation_is_transitive() {
        // c -> b -> a(ui). Excluding `a` must take out `b` and then `c`.
        let idx = index(vec![
            entry("UiThing", &[]),
            entry("Mid", &["UiThing"]),
            entry("Top", &["Mid"]),
            entry("Unrelated", &[]),
        ]);
        let f = filter(Filter {
            exclude: vec![r"^Ui".into()],
            exclude_dependents_of_excluded: true,
            ..base()
        });
        let d = f.apply(&idx);
        assert!(!d.keeps("UiThing"));
        assert!(!d.keeps("Mid"), "direct dependent must be dropped");
        assert!(!d.keeps("Top"), "transitive dependent must be dropped");
        assert!(d.keeps("Unrelated"));
    }

    #[test]
    fn included_modules_survive_dependency_propagation() {
        let idx = index(vec![
            entry("UiThing", &[]),
            entry("WAWebCore", &["UiThing"]),
        ]);
        let f = filter(Filter {
            include: vec![r"^WAWeb".into()],
            exclude: vec![r"^Ui".into()],
            exclude_dependents_of_excluded: true,
            ..base()
        });
        let d = f.apply(&idx);
        assert!(!d.keeps("UiThing"));
        assert!(
            d.keeps("WAWebCore"),
            "an explicitly included module is wanted regardless of what it imports"
        );
    }

    #[test]
    fn propagation_is_order_independent() {
        // A dependency cycle plus a dropped seed: a fixpoint converges, and the
        // answer must not depend on which module the walk happened to start from.
        let idx = index(vec![
            entry("UiThing", &[]),
            entry("A", &["B", "UiThing"]),
            entry("B", &["A"]),
        ]);
        let f = filter(Filter {
            exclude: vec![r"^Ui".into()],
            exclude_dependents_of_excluded: true,
            ..base()
        });
        let d = f.apply(&idx);
        assert!(!d.keeps("A") && !d.keeps("B"));
    }

    #[test]
    fn invalid_pattern_names_the_field_and_filter() {
        let err = FilterSet::compile(Filter {
            name: "mine".into(),
            exclude: vec!["(unclosed".into()],
            ..base()
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("mine") && err.contains("exclude"), "{err}");
    }

    #[test]
    fn noise_matchers_work() {
        let f = filter(Filter {
            noise_deps: vec![r"(?i)^react".into()],
            noise_code: vec![r"babelHelpers".into()],
            ..base()
        });
        assert!(f.is_noise_dep("React"));
        assert!(!f.is_noise_dep("WAWebFoo"));
        assert!(f.is_noise_line("  babelHelpers.extends(a, b);"));
    }
}
