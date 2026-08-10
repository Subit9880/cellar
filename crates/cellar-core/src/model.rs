//! The data model written to disk for every indexed bundle.
//!
//! Two artifacts describe a bundle, both JSON, both deterministic:
//!
//! - `manifest.json` — [`BundleManifest`]: identity (platform + revision), integrity
//!   (archive hash and size), counts, and extraction [`Diagnostics`].
//! - `index.json` — [`ModuleIndex`]: one [`ModuleEntry`] per module, sorted by name.
//!
//! The module sources themselves live beside them in `modules/`, one file per
//! module, named after the module — see [`crate::naming`].

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Version of the `manifest.json` / `index.json` contract.
///
/// Bumped independently of any bundle's revision: a consumer pins the schema it
/// understands, and WhatsApp shipping a new bundle never changes it.
pub const SCHEMA_VERSION: &str = "1.0.0";

/// A Meta first-party bundle target. These are the four values `btarchive` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Whatsapp,
    Messenger,
    Facebook,
    Instagram,
}

impl Platform {
    pub const ALL: [Platform; 4] = [
        Platform::Whatsapp,
        Platform::Messenger,
        Platform::Facebook,
        Platform::Instagram,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Whatsapp => "whatsapp",
            Platform::Messenger => "messenger",
            Platform::Facebook => "facebook",
            Platform::Instagram => "instagram",
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "whatsapp" | "wa" => Ok(Platform::Whatsapp),
            "messenger" | "msgr" => Ok(Platform::Messenger),
            "facebook" | "fb" => Ok(Platform::Facebook),
            "instagram" | "ig" => Ok(Platform::Instagram),
            other => Err(anyhow::anyhow!(
                "unknown platform {other:?} (expected one of: whatsapp, messenger, facebook, instagram)"
            )),
        }
    }
}

/// Identity of one bundle: a platform and the client revision it was served for.
///
/// This is also the directory name under `bundles/`, so it round-trips through
/// [`Display`](fmt::Display) and [`FromStr`].
///
/// It serializes as that same string — `"whatsapp-1030882912"` — rather than as a
/// two-field object. The string is the form every command line, error message and
/// directory name already uses, so a JSON consumer that has one can pass it
/// straight back without reassembling it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BundleId {
    pub platform: Platform,
    pub revision: u64,
}

impl Serialize for BundleId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BundleId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl BundleId {
    pub fn new(platform: Platform, revision: u64) -> Self {
        Self { platform, revision }
    }
}

impl fmt::Display for BundleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.platform, self.revision)
    }
}

impl FromStr for BundleId {
    type Err = anyhow::Error;

    /// Accepts `whatsapp-1030882912` and the bare `1030882912` (which defaults to
    /// WhatsApp, the overwhelmingly common case).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some((platform, revision)) = s.rsplit_once('-') {
            return Ok(BundleId {
                platform: platform.parse()?,
                revision: revision.parse().map_err(|_| {
                    anyhow::anyhow!("bundle id {s:?} has a non-numeric revision {revision:?}")
                })?,
            });
        }
        let revision: u64 = s.parse().map_err(|_| {
            anyhow::anyhow!("bundle id {s:?} is neither <platform>-<rev> nor <rev>")
        })?;
        Ok(BundleId::new(Platform::Whatsapp, revision))
    }
}

/// How the module sources in `modules/` were written.
///
/// The bundle ships minified onto single enormous lines. Storing that verbatim
/// makes the greppable directory nearly useless — one `grep` hit prints a
/// 200 KB line, and a line-based diff of two revisions is one changed line. So
/// the default is [`SourceForm::Pretty`]: modules are re-printed from the AST, which
/// makes both grep and diff work at statement granularity. Byte-level identity is
/// not lost, because [`ModuleEntry::raw_sha256`] fingerprints the original slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceForm {
    /// Re-printed from the oxc AST (multi-line, stable for a given codegen version).
    Pretty,
    /// The exact bytes of the `__d(...)` slice, as served.
    Raw,
}

/// One module extracted from a bundle.
///
/// Field order here is the field order in `index.json`; keep the identifying
/// fields first so a truncated read of a huge index is still useful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleEntry {
    /// Module name, the first argument of `__d("...", ...)`.
    pub name: String,
    /// Path of this module's source, relative to the bundle directory
    /// (`modules/<file>.js`). Always present, never guessed by the consumer: a
    /// name needing sanitization does not map back to its file by any rule the
    /// caller could apply itself. See [`crate::naming`].
    pub file: String,
    /// Declared dependencies — the string entries of the `__d` dependency array,
    /// in source order (not sorted: the array is positional, `d[0]` means the
    /// first entry, and re-ordering it would break that correspondence).
    pub deps: Vec<String>,
    /// Modules that declare this one as a dependency, sorted. Computed by the
    /// indexer, since a reverse edge cannot be read off a single module.
    pub dependents: Vec<String>,
    /// Names assigned onto the factory's `exports`/`module.exports` parameter,
    /// sorted. Resolved through the factory's actual parameter names rather than
    /// assumed to be `e`/`m`.
    pub exports: Vec<String>,
    /// Named functions declared at the top level of the factory body, sorted.
    pub functions: Vec<String>,
    /// SHA-256 of the exact `__d(...)` bytes as served. This is the change
    /// detector across revisions — unequal hash means the module really changed,
    /// independently of how it was re-printed.
    pub raw_sha256: String,
    /// Byte length of the original `__d(...)` slice.
    pub raw_len: u64,
    /// Byte length of the file actually written to `modules/`.
    pub stored_len: u64,
    /// Name of the archive member this module was found in, for provenance.
    pub chunk: String,
    /// How the stored source was written. Normally [`SourceForm::Pretty`]; falls
    /// back to [`SourceForm::Raw`] for a module the printer could not re-emit,
    /// which is also counted in [`Diagnostics::codegen_failures`].
    pub form: SourceForm,
    /// Other, textually different definitions of this same module name found
    /// elsewhere in the archive.
    ///
    /// These are real and common — around a quarter of modules in a WhatsApp Web
    /// bundle have one. The build ships a module more than once with genuinely
    /// different bodies (a variant compiled against `react-compiler-runtime`
    /// alongside one that is not, for instance), and which copy a page loads
    /// depends on the chunk it came from. They are recorded and written out rather
    /// than discarded, because a variant that appears or disappears between
    /// revisions is exactly the kind of rollout signal this tool exists to find.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<ModuleVariant>,
}

/// One additional definition of a module, beyond the primary at
/// [`ModuleEntry::file`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleVariant {
    pub sha256: String,
    /// Path relative to the bundle directory, alongside the primary.
    pub file: String,
    pub raw_len: u64,
    /// Dependencies this variant declares, when they differ from the primary's.
    /// The usual difference between variants, and the cheapest thing to compare.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps_added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps_removed: Vec<String>,
}

impl ModuleEntry {
    /// Whether more than one definition of this module exists in the archive.
    pub fn has_variants(&self) -> bool {
        !self.variants.is_empty()
    }

    /// The identity used to decide whether this module changed between revisions.
    ///
    /// Every definition counts, not just the primary. Comparing only the primary
    /// would miss a variant appearing or disappearing, and — worse — would report a
    /// spurious change whenever the choice of primary moved between two definitions
    /// that both already existed.
    pub fn fingerprint(&self) -> String {
        if self.variants.is_empty() {
            return self.raw_sha256.clone();
        }
        let mut all: Vec<&str> = std::iter::once(self.raw_sha256.as_str())
            .chain(self.variants.iter().map(|v| v.sha256.as_str()))
            .collect();
        all.sort_unstable();
        all.join("+")
    }
}

/// The full module index for one bundle: `index.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleIndex {
    pub schema_version: String,
    pub bundle: BundleId,
    /// Every module, sorted by name. Sorted-by-name is load-bearing: lookup is a
    /// binary search and two indexes can be diffed by a merge walk.
    pub modules: Vec<ModuleEntry>,
}

impl ModuleIndex {
    pub fn new(bundle: BundleId, mut modules: Vec<ModuleEntry>) -> Self {
        modules.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            bundle,
            modules,
        }
    }

    /// Look up one module by exact name.
    pub fn get(&self, name: &str) -> Option<&ModuleEntry> {
        self.modules
            .binary_search_by(|m| m.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.modules[i])
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn len(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

/// What the indexer saw but could not fully resolve.
///
/// Every counter here exists so that a WhatsApp-side change which breaks an
/// extraction step shows up as a number moving, instead of as a quietly emptier
/// index. A run with `modules_indexed > 0` and all-zero failure counters is a
/// clean extraction; anything else is reported by `cellar bundle info`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    /// Archive members read.
    pub chunks_read: u64,
    /// Archive members skipped as not-JavaScript (`.css`, source maps, …).
    pub chunks_skipped: u64,
    /// Archive members the JS parser could not parse at all. A non-zero value
    /// means modules are genuinely missing from the index.
    pub chunk_parse_failures: u64,
    /// Modules written as [`SourceForm::Raw`] because re-printing failed.
    pub codegen_failures: u64,
    /// Modules whose name collided under the filesystem's case-insensitivity or
    /// needed sanitizing, and so were stored under a disambiguated filename.
    pub renamed_files: u64,
    /// Module names with more than one textually distinct definition, and how
    /// many extra definitions each has. Byte-identical repeats are excluded — they
    /// are ordinary Metro output and would drown the signal. See
    /// [`ModuleEntry::variants`].
    pub modules_with_variants: BTreeMap<String, u64>,
    /// Dependency names referenced by some module but defined by none, sorted and
    /// capped. These are the graph's dangling edges (usually cross-bundle
    /// references) and are reported rather than silently dropped from the graph.
    pub unresolved_deps: Vec<String>,
    /// Total count of unresolved dependency names, before the sample above was capped.
    pub unresolved_deps_total: u64,
}

/// `manifest.json` — the identity and provenance of one stored bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleManifest {
    pub schema_version: String,
    pub bundle: BundleId,
    /// Where the archive came from. `None` for a bundle imported from a local
    /// directory, which is exactly the case where provenance is unknown and
    /// should not be invented.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// SHA-256 of the downloaded archive. Absent for a directory import, for the
    /// same reason: there was no archive to hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_len: Option<u64>,
    /// RFC 3339 UTC timestamp of when this bundle was indexed.
    pub indexed_at: String,
    /// Version of the `cellar` binary that produced this directory.
    pub cellar_version: String,
    /// Version of the JS printer used for [`SourceForm::Pretty`] sources. Two
    /// bundles indexed with different printer versions can still be diffed by
    /// `rawSha256`, but their pretty text may differ cosmetically — recorded so
    /// that difference is explainable rather than mysterious.
    pub codegen_version: String,
    pub source_form: SourceForm,
    /// Number of modules in `index.json`.
    pub modules_indexed: u64,
    /// Total bytes written under `modules/`.
    pub modules_bytes: u64,
    pub diagnostics: Diagnostics,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_id_round_trips() {
        let id = BundleId::new(Platform::Whatsapp, 1030882912);
        assert_eq!(id.to_string(), "whatsapp-1030882912");
        assert_eq!("whatsapp-1030882912".parse::<BundleId>().unwrap(), id);
        // A bare revision defaults to WhatsApp.
        assert_eq!("1030882912".parse::<BundleId>().unwrap(), id);
        assert_eq!(
            "messenger-42".parse::<BundleId>().unwrap(),
            BundleId::new(Platform::Messenger, 42)
        );
    }

    #[test]
    fn bundle_id_is_json_as_its_canonical_string() {
        let id = BundleId::new(Platform::Whatsapp, 1030882912);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"whatsapp-1030882912\"");
        assert_eq!(serde_json::from_str::<BundleId>(&json).unwrap(), id);
        // And a bad one fails deserialization rather than producing a wrong id.
        assert!(serde_json::from_str::<BundleId>("\"nosuch-1\"").is_err());
    }

    #[test]
    fn bundle_id_rejects_garbage() {
        assert!("whatsapp-abc".parse::<BundleId>().is_err());
        assert!("nosuch-12".parse::<BundleId>().is_err());
        assert!("hello".parse::<BundleId>().is_err());
    }

    #[test]
    fn index_sorts_and_looks_up() {
        let e = |name: &str| ModuleEntry {
            name: name.to_string(),
            file: format!("modules/{name}.js"),
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
        let idx = ModuleIndex::new(
            BundleId::new(Platform::Whatsapp, 1),
            vec![e("Zed"), e("Alpha"), e("Mid")],
        );
        assert_eq!(
            idx.modules
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Mid", "Zed"]
        );
        assert_eq!(idx.get("Mid").map(|m| m.name.as_str()), Some("Mid"));
        assert!(idx.get("Nope").is_none());
    }

    #[test]
    fn fingerprint_covers_every_definition_not_just_the_primary() {
        let variant = |sha: &str| ModuleVariant {
            sha256: sha.to_string(),
            file: format!("modules/M~alt-{sha}.js"),
            raw_len: 1,
            deps_added: vec![],
            deps_removed: vec![],
        };
        let entry = |primary: &str, variants: Vec<ModuleVariant>| ModuleEntry {
            name: "M".into(),
            file: "modules/M.js".into(),
            deps: vec![],
            dependents: vec![],
            exports: vec![],
            functions: vec![],
            raw_sha256: primary.into(),
            raw_len: 1,
            stored_len: 1,
            chunk: String::new(),
            form: SourceForm::Pretty,
            variants,
        };

        // A module with no variants fingerprints as its own hash.
        assert_eq!(entry("aaa", vec![]).fingerprint(), "aaa");

        // Gaining a second definition changes the fingerprint, even though the
        // primary is untouched — otherwise a new build variant appearing between
        // revisions would be reported as no change at all.
        assert_ne!(
            entry("aaa", vec![]).fingerprint(),
            entry("aaa", vec![variant("bbb")]).fingerprint()
        );

        // The fingerprint does not depend on which definition became primary, so a
        // reshuffle between two definitions that both already existed is not a
        // spurious change.
        assert_eq!(
            entry("aaa", vec![variant("bbb")]).fingerprint(),
            entry("bbb", vec![variant("aaa")]).fingerprint()
        );
    }
}
