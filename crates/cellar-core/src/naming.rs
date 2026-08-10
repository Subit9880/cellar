//! Mapping module names to filenames under `modules/`.
//!
//! The point of storing one file per module is that a caller can `ls`, `grep -r`
//! and open the directory directly, so the mapping is identity wherever it can be:
//! `WAWebSendMsgStanza` is `modules/WAWebSendMsgStanza.js`.
//!
//! Three things stop identity from always working, and all three are real in Meta
//! bundles rather than hypothetical:
//!
//! 1. **Path characters.** Module names include `/` (`fbjs/lib/invariant`) and can
//!    contain anything else a JS string can hold. `/` would silently create
//!    subdirectories; `..` would escape the bundle.
//! 2. **Case-insensitive filesystems.** APFS and NTFS are case-insensitive by
//!    default, so `WAWebFoo` and `WAWebfoo` — both of which occur — collide and the
//!    second write would clobber the first.
//! 3. **Name length.** A few generated names exceed the 255-byte per-component limit.
//!
//! In each case the name is sanitized and given a short hash of the *original*
//! name, so the result stays unique, stable across runs, and traceable back. The
//! index always records the resulting path, so no consumer needs to reimplement
//! any of this.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

/// Longest filename stem we will emit, in bytes. 255 is the usual per-component
/// limit; leaving room for the `.js` suffix and a disambiguating tag keeps the
/// total under it.
const MAX_STEM: usize = 200;

/// Length of the hex hash appended when a name must be disambiguated.
const TAG_LEN: usize = 10;

fn is_safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '@' | '+' | '$')
}

/// Whether `name` can be used as a filename stem verbatim.
fn is_plain(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_STEM
        && name.chars().all(is_safe_char)
        // A stem of dots would be `.` or `..`; a leading dot would hide the file
        // from a plain `ls` and from most glob patterns.
        && !name.starts_with('.')
}

fn short_hash(name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    hex::encode(digest)[..TAG_LEN].to_string()
}

fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if is_safe_char(c) { c } else { '_' })
        .collect();
    if out.starts_with('.') {
        out.insert(0, '_');
    }
    // Truncate on a char boundary, not a byte one.
    if out.len() > MAX_STEM {
        let mut end = MAX_STEM;
        while end > 0 && !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Assigns collision-free filenames to module names.
///
/// Feed names in sorted order for a deterministic result: the assignment depends
/// on which name is seen first, so sorted input makes the whole `modules/`
/// directory reproducible.
#[derive(Debug, Default)]
pub struct FileNamer {
    /// Lowercased stems already handed out — lowercased because the filesystem we
    /// are protecting against does not distinguish them.
    taken: HashSet<String>,
    renamed: u64,
}

impl FileNamer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of names that could not be stored verbatim.
    pub fn renamed(&self) -> u64 {
        self.renamed
    }

    /// Return the `modules/`-relative filename for `name`.
    pub fn file_for(&mut self, name: &str) -> String {
        let plain = is_plain(name);
        let mut stem = if plain {
            name.to_string()
        } else {
            format!("{}~{}", sanitize(name), short_hash(name))
        };

        // Even a plain name can collide case-insensitively with an earlier one; and
        // a sanitized stem carries a hash of the full name, so a second collision
        // there would mean two identical names, which cannot happen.
        if !self.taken.insert(stem.to_ascii_lowercase()) {
            stem = format!("{}~{}", sanitize(name), short_hash(name));
            self.taken.insert(stem.to_ascii_lowercase());
            self.renamed += 1;
        } else if !plain {
            self.renamed += 1;
        }

        format!("{stem}.js")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_identity() {
        let mut n = FileNamer::new();
        assert_eq!(n.file_for("WAWebSendMsgStanza"), "WAWebSendMsgStanza.js");
        assert_eq!(n.file_for("WAWebFoo.react"), "WAWebFoo.react.js");
        assert_eq!(n.file_for("path-to-regexp"), "path-to-regexp.js");
        assert_eq!(n.renamed(), 0);
    }

    #[test]
    fn path_separators_never_escape_the_directory() {
        use std::path::{Component, Path};

        let mut n = FileNamer::new();
        let f = n.file_for("fbjs/lib/invariant");
        assert!(f.starts_with("fbjs_lib_invariant~"));

        // The property that matters is that the result is exactly one normal path
        // component. Dots surviving inside the stem (`_.._..`) are harmless — a
        // component only traverses when it *is* `.` or `..`, not when it contains
        // them — and stripping them would collide `a..b` with `ab`.
        for name in [
            "fbjs/lib/invariant",
            "../../etc/passwd",
            "..",
            ".",
            "a/../b",
        ] {
            let f = n.file_for(name);
            let components: Vec<_> = Path::new(&f).components().collect();
            assert_eq!(
                components.len(),
                1,
                "{name:?} -> {f:?} must be a single component"
            );
            assert!(
                matches!(components[0], Component::Normal(_)),
                "{name:?} -> {f:?} must be a normal component, not a traversal"
            );
        }
    }

    #[test]
    fn case_insensitive_collisions_get_distinct_files() {
        let mut n = FileNamer::new();
        let a = n.file_for("WAWebFoo");
        let b = n.file_for("WAWebfoo");
        assert_eq!(a, "WAWebFoo.js");
        assert_ne!(a.to_ascii_lowercase(), b.to_ascii_lowercase());
        assert_eq!(n.renamed(), 1);
    }

    #[test]
    fn assignment_is_deterministic_for_the_same_input_order() {
        let names = ["Alpha", "alpha", "ALPHA"];
        let run = || {
            let mut n = FileNamer::new();
            names.iter().map(|s| n.file_for(s)).collect::<Vec<_>>()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn overlong_names_are_truncated_and_stay_unique() {
        let mut n = FileNamer::new();
        let long_a = "W".repeat(400) + "A";
        let long_b = "W".repeat(400) + "B";
        let a = n.file_for(&long_a);
        let b = n.file_for(&long_b);
        assert!(a.len() <= 255 && b.len() <= 255);
        assert_ne!(a, b, "distinct long names must not collapse onto one file");
    }

    #[test]
    fn dotfiles_are_not_hidden() {
        let mut n = FileNamer::new();
        assert!(!n.file_for(".hidden").starts_with('.'));
    }
}
