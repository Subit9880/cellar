//! Getting bundles: revision discovery, `btarchive` download, and unpacking.
//!
//! Meta serves a full, historical source archive per client revision at
//! `https://www.facebook.com/btarchive/<revision>/<platform>` — a zip of the chunk
//! files that make up the web client at that revision. That endpoint is what makes
//! this whole tool possible: the live bundle URLs 404 as soon as a rollout moves
//! on, so a revision you did not capture at the time is gone, whereas `btarchive`
//! still answers for old revisions. Diffing "what changed between two releases"
//! only works because of that.
//!
//! Nothing here writes into the archive; it produces an extracted chunk directory
//! that `cellar-index` consumes.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cellar_core::model::{BundleId, Platform};
use regex::Regex;
use sha2::{Digest, Sha256};

/// A browser-shaped User-Agent. The revision page varies its markup by client, and
/// a default library agent gets a page without the version field at all.
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/125.0.0.0 Safari/537.36";

/// The headers a real Chrome top-level navigation sends, minus `Accept-Encoding`
/// (left to the HTTP client, which has to decode whatever it negotiates).
///
/// These are **not** cosmetic, and getting them wrong is not a soft failure.
/// Meta's edge cross-checks the User-Agent against the rest of the request: a
/// request claiming Chrome 125 while omitting `Sec-Fetch-*` is definitionally not
/// Chrome, since every Chrome since 76 sends fetch metadata on every request, and
/// the edge answers it `400`. Measured against
/// `https://www.facebook.com/btarchive/<rev>/whatsapp`, logged out:
///
/// | request                        | status |
/// |--------------------------------|--------|
/// | no `User-Agent` override       | 302    |
/// | Chrome UA alone                | 400    |
/// | Chrome UA + `Accept-Language`  | 400    |
/// | Chrome UA + `sec-ch-ua`        | 400    |
/// | Chrome UA + `Accept` (html)    | 400    |
/// | **Chrome UA + `Sec-Fetch-*`**  | **302**|
///
/// So `Sec-Fetch-*` is the discriminator; the rest are sent because a browser
/// sends them and the point is to be a browser, not to send the minimum that
/// currently passes.
///
/// This mirrors how Meta's own Code Verify extension reaches this endpoint — it
/// does not `fetch()` it at all, it calls `window.open()`, i.e. a real top-level
/// navigation. `Sec-Fetch-Site: none` is what that produces for a new tab with no
/// initiator, which is the case being imitated.
const NAVIGATION_HEADERS: &[(&str, &str)] = &[
    (
        "Accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
    ),
    ("Accept-Language", "en-US,en;q=0.9"),
    ("Upgrade-Insecure-Requests", "1"),
    (
        "sec-ch-ua",
        "\"Chromium\";v=\"125\", \"Not.A/Brand\";v=\"24\"",
    ),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"macOS\""),
    ("Sec-Fetch-Dest", "document"),
    ("Sec-Fetch-Mode", "navigate"),
    ("Sec-Fetch-Site", "none"),
    ("Sec-Fetch-User", "?1"),
];

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Guard against a truncated or error-page response being indexed as a bundle. A
/// real archive is tens of megabytes; anything this small is a failure page.
const MIN_ARCHIVE_BYTES: u64 = 64 * 1024;

/// The archive URL for one bundle.
pub fn archive_url(id: BundleId) -> String {
    format!(
        "https://www.facebook.com/btarchive/{}/{}",
        id.revision, id.platform
    )
}

/// Result of a download.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: String,
    pub sha256: String,
    pub len: u64,
    /// Archive members written to the destination.
    pub members: u64,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

/// A GET carrying the full browser-navigation header set.
///
/// Every request this crate makes to Meta goes through here. Both endpoints are
/// behind the same edge check, so there is no such thing as a request that only
/// "sort of" needs these — and a second code path that built its headers by hand
/// is exactly how the download ended up sending `Accept: */*` and nothing else
/// while revision discovery sent the full set and worked.
fn navigate(agent: &ureq::Agent, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let mut request = agent.get(url);
    for (name, value) in NAVIGATION_HEADERS {
        request = request.header(*name, *value);
    }
    request
}

/// Discover the current client revision for `platform`.
///
/// Only WhatsApp is supported: its web entry page inlines `"client_revision":N`.
/// The other platforms have no comparable unauthenticated source, and guessing one
/// would produce a confidently wrong revision, so they ask for `--rev` instead.
pub fn latest_revision(platform: Platform) -> Result<u64> {
    if platform != Platform::Whatsapp {
        bail!(
            "revision discovery is only implemented for whatsapp; \
             pass an explicit --rev for {platform}"
        );
    }
    let agent = agent();
    let body = navigate(&agent, "https://web.whatsapp.com/")
        .call()
        .context("requesting web.whatsapp.com for the current revision")?
        .body_mut()
        .read_to_string()
        .context("reading the web.whatsapp.com response")?;

    let re = Regex::new(r#""client_revision"\s*:\s*(\d+)"#).expect("static pattern");
    let caps = re.captures(&body).context(
        "no `client_revision` in the web.whatsapp.com response — the page shape has changed; \
         pass an explicit --rev",
    )?;
    caps[1].parse().context("client_revision was not a number")
}

/// Download the archive for `id` and extract it into `dest`.
///
/// The archive is streamed to a temporary file rather than buffered: these run to
/// hundreds of megabytes, and holding one in memory alongside the extraction is
/// avoidable waste.
pub fn download_and_extract(id: BundleId, dest: &Path) -> Result<Fetched> {
    let url = archive_url(id);
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;

    let tmp = dest.join(".archive.zip.part");
    let (sha256, len) = stream_to_file(&url, &tmp)?;

    if len < MIN_ARCHIVE_BYTES {
        let _ = fs::remove_file(&tmp);
        bail!(
            "{url} returned only {len} bytes — revision {} may not exist for {} \
             (Meta answers an unknown revision with an error page, not a 404)",
            id.revision,
            id.platform
        );
    }

    let members =
        extract_zip(&tmp, dest).with_context(|| format!("unpacking the archive for {id}"))?;
    fs::remove_file(&tmp).ok();

    Ok(Fetched {
        url,
        sha256,
        len,
        members,
    })
}

/// GET `url` into `path`, returning `(sha256, bytes)`. Hashing happens on the way
/// past, so the file is never read a second time.
fn stream_to_file(url: &str, path: &Path) -> Result<(String, u64)> {
    let agent = agent();
    let mut response = navigate(&agent, url)
        .call()
        .with_context(|| format!("requesting {url}"))?;

    let status = response.status();
    if !status.is_success() {
        // 400 here is the signature of a request the edge did not believe: see
        // NAVIGATION_HEADERS. Say so, because the status alone reads like a
        // problem with the revision rather than with the request.
        if status.as_u16() == 400 {
            bail!(
                "{url} responded 400 — Meta's edge rejects requests whose headers \
                 contradict their User-Agent. This is a bug in cellar's request \
                 shape, not a bad revision; see NAVIGATION_HEADERS in cellar-fetch."
            );
        }
        bail!("{url} responded {status}");
    }

    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;

    loop {
        let n = reader.read(&mut buf).context("reading the response body")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", path.display()))?;
        total += n as u64;
    }
    file.sync_all()?;

    Ok((hex::encode(hasher.finalize()), total))
}

/// Extract every file member of a zip into `dest`, flattened.
///
/// Flattening is deliberate: the chunk files are content-addressed and their names
/// are already unique, and a flat directory is what both the indexer and a person
/// running `grep -r` want. Directory entries are skipped, and any member whose path
/// would escape `dest` is refused rather than sanitized, because in this archive
/// such a member would mean the input is not what we think it is.
pub fn extract_zip(archive: &Path, dest: &Path) -> Result<u64> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a readable zip archive", archive.display()))?;

    let mut written = 0u64;
    for i in 0..zip.len() {
        let mut member = zip
            .by_index(i)
            .with_context(|| format!("reading member {i}"))?;
        if !member.is_file() {
            continue;
        }
        let Some(path) = member.enclosed_name() else {
            bail!(
                "archive member {:?} has a path that escapes the destination",
                member.name()
            );
        };
        let Some(name) = path.file_name() else {
            continue;
        };
        let out_path = dest.join(name);
        let mut out =
            File::create(&out_path).with_context(|| format!("creating {}", out_path.display()))?;
        io::copy(&mut member, &mut out)
            .with_context(|| format!("writing {}", out_path.display()))?;
        written += 1;
    }

    if written == 0 {
        bail!("archive {} contained no files", archive.display());
    }
    Ok(written)
}

/// Hash an existing file, for verifying an imported archive.
pub fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

/// Where an extraction should stage its chunks: a sibling of the bundle directory,
/// so the two are on the same filesystem and the eventual cleanup is cheap.
pub fn staging_dir(bundle_dir: &Path) -> PathBuf {
    bundle_dir.join(".chunks")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn archive_url_is_revision_and_platform() {
        assert_eq!(
            archive_url(BundleId::new(Platform::Whatsapp, 1030882912)),
            "https://www.facebook.com/btarchive/1030882912/whatsapp"
        );
        assert_eq!(
            archive_url(BundleId::new(Platform::Instagram, 42)),
            "https://www.facebook.com/btarchive/42/instagram"
        );
    }

    #[test]
    fn navigation_headers_carry_the_fetch_metadata_the_edge_checks() {
        // Meta answers a Chrome User-Agent that omits `Sec-Fetch-*` with a 400.
        // Dropping any of these silently breaks every download while revision
        // discovery keeps working, which is precisely how this shipped broken.
        let names: Vec<&str> = NAVIGATION_HEADERS.iter().map(|(n, _)| *n).collect();
        for required in [
            "Sec-Fetch-Dest",
            "Sec-Fetch-Mode",
            "Sec-Fetch-Site",
            "Sec-Fetch-User",
        ] {
            assert!(names.contains(&required), "missing {required}");
        }
        assert!(
            USER_AGENT.contains("Chrome/"),
            "the UA is what triggers the check"
        );
    }

    #[test]
    fn navigation_headers_are_internally_consistent() {
        let get = |name: &str| {
            NAVIGATION_HEADERS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
        };
        // A top-level navigation, which is what `window.open` produces and what
        // the `Accept` value has to agree with.
        assert_eq!(get("Sec-Fetch-Mode"), Some("navigate"));
        assert_eq!(get("Sec-Fetch-Dest"), Some("document"));
        assert!(get("Accept").is_some_and(|a| a.starts_with("text/html")));

        // Accept-Encoding must stay out: the HTTP client negotiates it and has to
        // be able to decode whatever comes back. Pinning it here would make the
        // client claim support for encodings it will not decode.
        assert!(get("Accept-Encoding").is_none());

        // No duplicates — ureq would send the header twice.
        let mut names: Vec<&str> = NAVIGATION_HEADERS.iter().map(|(n, _)| *n).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    #[test]
    fn revision_discovery_refuses_to_guess_for_other_platforms() {
        let err = latest_revision(Platform::Facebook).unwrap_err().to_string();
        assert!(err.contains("--rev"), "{err}");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cellar-fetch-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build a zip in memory and write it to `path`.
    fn write_zip(path: &Path, members: &[(&str, &str)]) {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (name, body) in members {
                w.start_file(*name, opts).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        fs::write(path, &buf).unwrap();
    }

    #[test]
    fn extraction_flattens_and_counts_members() {
        let dir = temp_dir("extract");
        let archive = dir.join("a.zip");
        write_zip(
            &archive,
            &[
                ("bundles/abc.js", "__d(\"A\",[],function(){});"),
                ("def.css", ".a{}"),
            ],
        );
        let n = extract_zip(&archive, &dir).unwrap();
        assert_eq!(n, 2);
        assert!(dir.join("abc.js").is_file(), "nested paths are flattened");
        assert!(dir.join("def.css").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_archive_is_an_error_not_an_empty_bundle() {
        let dir = temp_dir("empty");
        let archive = dir.join("a.zip");
        write_zip(&archive, &[]);
        let err = extract_zip(&archive, &dir).unwrap_err().to_string();
        assert!(err.contains("no files"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hashing_matches_a_known_digest() {
        let dir = temp_dir("hash");
        let f = dir.join("x");
        fs::write(&f, b"abc").unwrap();
        let (sha, len) = hash_file(&f).unwrap();
        assert_eq!(len, 3);
        assert_eq!(
            sha,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
