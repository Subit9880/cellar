//! The on-disk archive.
//!
//! ```text
//! $CELLAR_HOME/                     (default: ~/.cellar)
//!   filters/
//!     <name>.json                   user filters; built-ins are compiled in
//!   bundles/
//!     whatsapp-1030882912/
//!       manifest.json               identity, integrity, counts, diagnostics
//!       index.json                  every module, sorted by name
//!       modules/
//!         WAWebSendMsgStanza.js     one file per module, named after the module
//!         …
//! ```
//!
//! `modules/` is a deliberate, first-class part of the interface rather than an
//! implementation detail. The intended consumer is an agent that already has file
//! reading and recursive grep, so the most useful thing the archive can do is hand
//! it a directory those tools work on directly: real filenames, real line
//! structure, no query language in the way. Everything this crate returns therefore
//! carries the path of the source it is talking about, so any answer can be taken
//! over by the caller's own tools mid-investigation.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::builtin;
use crate::diff::SourceLoader;
use crate::filter::{Filter, FilterSet};
use crate::model::{BundleId, BundleManifest, ModuleEntry, ModuleIndex, Platform};

const BUNDLES: &str = "bundles";
const FILTERS: &str = "filters";
const MODULES: &str = "modules";
const MANIFEST: &str = "manifest.json";
const INDEX: &str = "index.json";

/// The archive root.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open (creating if needed) the archive at `root`, or at `$CELLAR_HOME`,
    /// or at `~/.cellar`.
    pub fn open(root: Option<PathBuf>) -> Result<Self> {
        let root = match root {
            Some(r) => r,
            None => match std::env::var_os("CELLAR_HOME") {
                Some(r) => PathBuf::from(r),
                None => {
                    let home = std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .context("neither CELLAR_HOME nor HOME is set; pass --root explicitly")?;
                    home.join(".cellar")
                }
            },
        };
        fs::create_dir_all(root.join(BUNDLES))
            .with_context(|| format!("creating archive at {}", root.display()))?;
        fs::create_dir_all(root.join(FILTERS))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn bundles_dir(&self) -> PathBuf {
        self.root.join(BUNDLES)
    }

    pub fn filters_dir(&self) -> PathBuf {
        self.root.join(FILTERS)
    }

    pub fn bundle_dir(&self, id: BundleId) -> PathBuf {
        self.bundles_dir().join(id.to_string())
    }

    // --- bundles -----------------------------------------------------------

    /// Every fully indexed bundle, sorted by platform then revision.
    ///
    /// A directory without a readable `manifest.json` is skipped: it is a partial
    /// or interrupted download, not a bundle.
    pub fn list_bundles(&self) -> Result<Vec<BundleHandle>> {
        let mut out = Vec::new();
        let dir = self.bundles_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(id) = name.parse::<BundleId>() else {
                continue;
            };
            if let Ok(handle) = self.open_bundle(id) {
                out.push(handle);
            }
        }
        out.sort_by_key(|h| h.id);
        Ok(out)
    }

    pub fn open_bundle(&self, id: BundleId) -> Result<BundleHandle> {
        let dir = self.bundle_dir(id);
        let manifest_path = dir.join(MANIFEST);
        let bytes = fs::read(&manifest_path).with_context(|| {
            format!(
                "bundle {id} is not in the archive (no {})",
                manifest_path.display()
            )
        })?;
        let manifest: BundleManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        Ok(BundleHandle { id, dir, manifest })
    }

    pub fn has_bundle(&self, id: BundleId) -> bool {
        self.bundle_dir(id).join(MANIFEST).is_file()
    }

    /// Resolve a user-supplied bundle spec.
    ///
    /// Accepts `whatsapp-1030882912`, a bare `1030882912`, `latest` (the highest
    /// revision held for WhatsApp), and `<platform>-latest`.
    pub fn resolve(&self, spec: &str) -> Result<BundleId> {
        if let Some(platform) = spec.strip_suffix("-latest") {
            return self.latest(platform.parse()?);
        }
        if spec == "latest" {
            return self.latest(Platform::Whatsapp);
        }
        let id: BundleId = spec.parse()?;
        if !self.has_bundle(id) {
            bail!(
                "bundle {id} is not in the archive — run `cellar bundle add --platform {} --rev {}`",
                id.platform,
                id.revision
            );
        }
        Ok(id)
    }

    /// The highest-revision bundle held for `platform`.
    pub fn latest(&self, platform: Platform) -> Result<BundleId> {
        self.list_bundles()?
            .into_iter()
            .filter(|h| h.id.platform == platform)
            .map(|h| h.id)
            .max_by_key(|id| id.revision)
            .with_context(|| format!("no {platform} bundle in the archive yet"))
    }

    /// The two highest-revision bundles for `platform`, oldest first — the default
    /// operands for `cellar diff` with no arguments.
    pub fn latest_pair(&self, platform: Platform) -> Result<(BundleId, BundleId)> {
        let mut ids: Vec<BundleId> = self
            .list_bundles()?
            .into_iter()
            .filter(|h| h.id.platform == platform)
            .map(|h| h.id)
            .collect();
        ids.sort_by_key(|id| id.revision);
        if ids.len() < 2 {
            bail!(
                "need at least two {platform} bundles to diff; the archive has {}",
                ids.len()
            );
        }
        let new = ids.pop().expect("checked len >= 2");
        let old = ids.pop().expect("checked len >= 2");
        Ok((old, new))
    }

    /// Delete a bundle and everything under it.
    pub fn remove_bundle(&self, id: BundleId) -> Result<()> {
        let dir = self.bundle_dir(id);
        if !dir.exists() {
            bail!("bundle {id} is not in the archive");
        }
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        Ok(())
    }

    /// Prepare a bundle directory for writing, clearing any previous contents.
    ///
    /// Returns the directory. The caller (the indexer) writes `modules/` into it and
    /// finishes with [`Store::commit_bundle`].
    pub fn begin_bundle(&self, id: BundleId) -> Result<PathBuf> {
        let dir = self.bundle_dir(id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("clearing previous {}", dir.display()))?;
        }
        fs::create_dir_all(dir.join(MODULES))
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(dir)
    }

    /// Write `index.json` and `manifest.json`, making the bundle visible to
    /// [`Store::list_bundles`].
    ///
    /// The manifest is written last and by rename, so a crash mid-write leaves a
    /// directory that is skipped as incomplete rather than one that loads with a
    /// truncated index.
    pub fn commit_bundle(
        &self,
        id: BundleId,
        index: &ModuleIndex,
        manifest: &BundleManifest,
    ) -> Result<BundleHandle> {
        let dir = self.bundle_dir(id);
        write_json_atomic(&dir.join(INDEX), index)?;
        write_json_atomic(&dir.join(MANIFEST), manifest)?;
        self.open_bundle(id)
    }

    // --- filters -----------------------------------------------------------

    /// Every filter: the compiled-in ones plus everything in `filters/`.
    ///
    /// A user filter that shadows a built-in name wins, so a built-in can be
    /// overridden without editing this binary.
    pub fn list_filters(&self) -> Result<Vec<Filter>> {
        let mut by_name: BTreeMap<String, Filter> = builtin::all()
            .into_iter()
            .map(|f| (f.name.clone(), f))
            .collect();

        let dir = self.filters_dir();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let bytes = fs::read(&path)?;
                let mut f: Filter = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing filter {}", path.display()))?;
                // The filename is the identity; a mismatched `name` field would
                // make `filter get` and `filter delete` disagree.
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    f.name = stem.to_string();
                }
                f.builtin = false;
                by_name.insert(f.name.clone(), f);
            }
        }
        Ok(by_name.into_values().collect())
    }

    pub fn get_filter(&self, name: &str) -> Result<Filter> {
        let path = self.filter_path(name);
        if path.is_file() {
            let bytes = fs::read(&path)?;
            let mut f: Filter = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing filter {}", path.display()))?;
            f.name = name.to_string();
            f.builtin = false;
            return Ok(f);
        }
        builtin::get(name).with_context(|| {
            let known: Vec<String> = self
                .list_filters()
                .unwrap_or_default()
                .into_iter()
                .map(|f| f.name)
                .collect();
            format!("no filter named {name:?} (known: {})", known.join(", "))
        })
    }

    /// Load and compile a filter in one step.
    pub fn compiled_filter(&self, name: &str) -> Result<FilterSet> {
        FilterSet::compile(self.get_filter(name)?)
    }

    /// Create or replace a user filter.
    ///
    /// Rejected if it does not compile: a stored filter that cannot be applied
    /// would fail later, at the point of use, with no hint of where it came from.
    pub fn put_filter(&self, mut filter: Filter) -> Result<()> {
        if filter.name.is_empty()
            || !filter
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            bail!(
                "filter name {:?} must be non-empty and use only letters, digits, `-` and `_`",
                filter.name
            );
        }
        filter.builtin = false;
        // Compile before writing, and discard the result: this is a validation step.
        FilterSet::compile(filter.clone())?;
        fs::create_dir_all(self.filters_dir())?;
        write_json_atomic(&self.filter_path(&filter.name), &filter)
    }

    /// Delete a user filter. Built-ins cannot be deleted; shadowing one just
    /// restores the built-in.
    pub fn delete_filter(&self, name: &str) -> Result<()> {
        let path = self.filter_path(name);
        if !path.is_file() {
            if builtin::get(name).is_some() {
                bail!(
                    "{name:?} is a built-in filter and has not been overridden; \
                     `cellar filter fork {name} <new-name>` to make an editable copy"
                );
            }
            bail!("no user filter named {name:?}");
        }
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        Ok(())
    }

    fn filter_path(&self, name: &str) -> PathBuf {
        self.filters_dir().join(format!("{name}.json"))
    }
}

/// A bundle in the archive.
#[derive(Debug, Clone)]
pub struct BundleHandle {
    pub id: BundleId,
    pub dir: PathBuf,
    pub manifest: BundleManifest,
}

impl BundleHandle {
    pub fn modules_dir(&self) -> PathBuf {
        self.dir.join(MODULES)
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX)
    }

    /// Read and parse `index.json`.
    pub fn index(&self) -> Result<ModuleIndex> {
        let path = self.index_path();
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    /// Absolute path of one module's source.
    pub fn module_path(&self, entry: &ModuleEntry) -> PathBuf {
        self.dir.join(&entry.file)
    }

    pub fn read_module(&self, entry: &ModuleEntry) -> Result<String> {
        let path = self.module_path(entry);
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
    }
}

impl SourceLoader for BundleHandle {
    fn load(&self, entry: &ModuleEntry) -> Result<String> {
        self.read_module(entry)
    }
}

/// Serialize to pretty JSON via a temporary file and a rename.
///
/// The rename is atomic within a directory, so a reader never sees a half-written
/// index — which matters because indexing takes minutes and is routinely
/// interrupted.
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::Verdict;
    use crate::model::{Diagnostics, SCHEMA_VERSION, SourceForm};

    fn temp_root() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "cellar-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&base);
        base
    }

    fn manifest(id: BundleId, modules: u64) -> BundleManifest {
        BundleManifest {
            schema_version: SCHEMA_VERSION.into(),
            bundle: id,
            source_url: None,
            archive_sha256: None,
            archive_len: None,
            indexed_at: "2026-01-01T00:00:00Z".into(),
            cellar_version: "test".into(),
            codegen_version: "test".into(),
            source_form: SourceForm::Pretty,
            modules_indexed: modules,
            modules_bytes: 0,
            diagnostics: Diagnostics::default(),
        }
    }

    fn seed(store: &Store, id: BundleId) -> BundleHandle {
        store.begin_bundle(id).unwrap();
        let index = ModuleIndex::new(id, vec![]);
        store.commit_bundle(id, &index, &manifest(id, 0)).unwrap()
    }

    #[test]
    fn bundles_round_trip_and_list_sorted() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();
        assert!(store.list_bundles().unwrap().is_empty());

        seed(&store, BundleId::new(Platform::Whatsapp, 200));
        seed(&store, BundleId::new(Platform::Whatsapp, 100));
        seed(&store, BundleId::new(Platform::Messenger, 50));

        let ids: Vec<String> = store
            .list_bundles()
            .unwrap()
            .into_iter()
            .map(|h| h.id.to_string())
            .collect();
        assert_eq!(ids, ["whatsapp-100", "whatsapp-200", "messenger-50"]);

        assert_eq!(
            store.resolve("latest").unwrap(),
            BundleId::new(Platform::Whatsapp, 200)
        );
        assert_eq!(
            store.resolve("messenger-latest").unwrap(),
            BundleId::new(Platform::Messenger, 50)
        );
        assert_eq!(
            store.latest_pair(Platform::Whatsapp).unwrap(),
            (
                BundleId::new(Platform::Whatsapp, 100),
                BundleId::new(Platform::Whatsapp, 200)
            )
        );

        store
            .remove_bundle(BundleId::new(Platform::Whatsapp, 100))
            .unwrap();
        assert!(!store.has_bundle(BundleId::new(Platform::Whatsapp, 100)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_a_bundle() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();
        // An interrupted download: the directory exists, the manifest does not.
        store
            .begin_bundle(BundleId::new(Platform::Whatsapp, 7))
            .unwrap();
        assert!(store.list_bundles().unwrap().is_empty());
        assert!(store.resolve("whatsapp-7").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_names_the_fix_when_a_bundle_is_absent() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();
        let err = store.resolve("whatsapp-999").unwrap_err().to_string();
        assert!(err.contains("bundle add"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filters_round_trip_and_user_shadows_builtin() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();

        assert!(store.get_filter("default").unwrap().builtin);

        let mine = Filter {
            name: "mine".into(),
            description: "test".into(),
            hard_exclude: vec![],
            include: vec![r"^WAWeb".into()],
            exclude: vec![],
            default_verdict: Verdict::Drop,
            exclude_dependents_of_excluded: false,
            noise_deps: vec![],
            noise_code: vec![],
            builtin: false,
        };
        store.put_filter(mine.clone()).unwrap();
        assert_eq!(store.get_filter("mine").unwrap().include, mine.include);
        assert!(
            store
                .list_filters()
                .unwrap()
                .iter()
                .any(|f| f.name == "mine")
        );

        // Shadowing a built-in name replaces it in listings.
        let mut shadow = mine.clone();
        shadow.name = "default".into();
        store.put_filter(shadow).unwrap();
        assert!(!store.get_filter("default").unwrap().builtin);

        store.delete_filter("default").unwrap();
        assert!(
            store.get_filter("default").unwrap().builtin,
            "deleting the override restores the built-in"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_filter_that_does_not_compile_is_rejected_at_write_time() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();
        let bad = Filter {
            name: "bad".into(),
            description: String::new(),
            hard_exclude: vec![],
            include: vec!["(unclosed".into()],
            exclude: vec![],
            default_verdict: Verdict::Keep,
            exclude_dependents_of_excluded: false,
            noise_deps: vec![],
            noise_code: vec![],
            builtin: false,
        };
        assert!(store.put_filter(bad).is_err());
        assert!(!store.filter_path("bad").exists(), "nothing was written");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn deleting_a_builtin_explains_fork() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();
        let err = store.delete_filter("default").unwrap_err().to_string();
        assert!(err.contains("fork"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filter_names_are_constrained_to_safe_characters() {
        let root = temp_root();
        let store = Store::open(Some(root.clone())).unwrap();
        for bad in ["", "../escape", "has space", "dot.dot"] {
            let f = Filter {
                name: bad.into(),
                description: String::new(),
                hard_exclude: vec![],
                include: vec![],
                exclude: vec![],
                default_verdict: Verdict::Keep,
                exclude_dependents_of_excluded: false,
                noise_deps: vec![],
                noise_code: vec![],
                builtin: false,
            };
            assert!(store.put_filter(f).is_err(), "{bad:?} must be rejected");
        }
        let _ = fs::remove_dir_all(&root);
    }
}
