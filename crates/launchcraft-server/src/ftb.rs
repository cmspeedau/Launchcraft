//! FTB (Feed The Beast) provider for the unified modpack browser.
//!
//! Uses the legacy pack catalog that PrismLauncher also reads: two XML
//! files on the `dist.creeper.host/FTB2` CDN (`static/modpacks.xml` and
//! `static/thirdparty.xml`) describing every public pack, with plain zip
//! downloads at `modpacks/<dir>/<version_underscores>/<file>`. Those
//! archives are *baked* game trees (`minecraft/…` with jars and configs,
//! no manifest), so the importer copies them straight into the workspace
//! and this catalog supplies the name/version/mcVersion metadata.
//!
//! Catalog handles: a project is the pack `dir`, a version is the version
//! string itself (`1.1.0`), which maps to `1_1_0` in the download path.

use std::collections::HashSet;

use axum::http::StatusCode;

use crate::modpacks::{fetch_bytes, FallbackMeta, Found, PackVersion, SearchArgs};
use crate::{ApiError, AppState};

/// Search page size; the console pages by this.
pub(crate) const PAGE_SIZE: u32 = 20;

/// One `<modpack>` entry of the legacy catalog.
struct FtbPack {
    name: String,
    author: String,
    dir: String,
    /// Zip filename within the version directory.
    file: String,
    version: String,
    old_versions: Vec<String>,
    mc_version: String,
    logo: String,
    description: String,
}

/// Both catalog files, public + third-party, parsed once per request.
async fn catalog(state: &AppState) -> Result<Vec<FtbPack>, ApiError> {
    let mut packs = Vec::new();
    for page in ["modpacks.xml", "thirdparty.xml"] {
        let url = format!("{}/static/{page}", state.ftb_base);
        let bytes = fetch_bytes(state, &url).await?;
        let xml = String::from_utf8_lossy(&bytes);
        packs.extend(parse_modpacks(&xml));
    }
    if packs.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "FTB catalog contained no packs (is the CDN URL right?)",
        ));
    }
    Ok(packs)
}

fn find_pack<'a>(packs: &'a [FtbPack], dir: &str) -> Result<&'a FtbPack, ApiError> {
    packs
        .iter()
        .find(|p| p.dir == dir)
        .ok_or_else(|| ApiError::bad_request(format!("unknown FTB pack: {dir}")))
}

/// All versions, current first: the `version` attribute plus `oldVersions`
/// (which repeats the current one; dedup keeps the order).
fn all_versions(p: &FtbPack) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for v in std::iter::once(p.version.as_str()).chain(p.old_versions.iter().map(String::as_str)) {
        if !v.is_empty() && seen.insert(v.to_string()) {
            out.push(v.to_string());
        }
    }
    out
}

/// `GET /console/modpacks/search?provider=ftb` backend.
pub(crate) async fn search(
    state: &AppState,
    args: &SearchArgs<'_>,
) -> Result<(Vec<Found>, u64), ApiError> {
    let packs = catalog(state).await?;
    let needle = args.q.map(str::to_lowercase);
    let mut hits: Vec<&FtbPack> = packs
        .iter()
        .filter(|p| {
            args.game_version
                .is_none_or(|gv| gv.eq_ignore_ascii_case(&p.mc_version))
                && needle.as_deref().is_none_or(|q| {
                    p.name.to_lowercase().contains(q)
                        || p.author.to_lowercase().contains(q)
                        || p.description.to_lowercase().contains(q)
                })
        })
        .collect();
    if args.sort == "name" {
        hits.sort_by_key(|p| p.name.to_lowercase());
    }
    let total = hits.len() as u64;
    let page = hits
        .into_iter()
        .skip(args.index as usize * PAGE_SIZE as usize)
        .take(PAGE_SIZE as usize)
        .map(|p| Found {
            id: p.dir.clone(),
            name: p.name.clone(),
            description: p.description.clone(),
            icon: format!("{}/static/{}", state.ftb_base, p.logo),
            downloads: 0, // the legacy catalog carries no download counts
        })
        .collect();
    Ok((page, total))
}

/// `GET /console/modpacks/versions?provider=ftb` backend, current first.
pub(crate) async fn versions(
    state: &AppState,
    project: &str,
) -> Result<Vec<PackVersion>, ApiError> {
    let packs = catalog(state).await?;
    let pack = find_pack(&packs, project.trim())?;
    Ok(all_versions(pack)
        .into_iter()
        .map(|v| PackVersion {
            date: String::new(), // the catalog has no release dates
            size: 0,
            game_versions: vec![pack.mc_version.clone()],
            name: v.clone(),
            id: v,
        })
        .collect())
}

/// The catalog's current version for a pack.
pub(crate) async fn latest(state: &AppState, project: &str) -> Result<String, ApiError> {
    let packs = catalog(state).await?;
    let pack = find_pack(&packs, project.trim())?;
    if pack.version.is_empty() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("FTB catalog lists no current version for {}", pack.dir),
        ));
    }
    Ok(pack.version.clone())
}

/// Fetch a baked pack zip: `modpacks/<dir>/<1_1_0>/<file>.zip`.
/// Metadata comes from the catalog because the archive has no manifest.
pub(crate) async fn download(
    state: &AppState,
    project: &str,
    file: &str,
) -> Result<(Vec<u8>, Option<FallbackMeta>), ApiError> {
    let packs = catalog(state).await?;
    let pack = find_pack(&packs, project.trim())?;
    let version = file.trim();
    if !all_versions(pack).iter().any(|v| v == version) {
        return Err(ApiError::bad_request(format!(
            "FTB pack {} has no version {version}",
            pack.dir
        )));
    }
    let url = format!(
        "{}/modpacks/{}/{}/{}",
        state.ftb_base,
        pack.dir,
        version.replace('.', "_"),
        pack.file
    );
    let bytes = fetch_bytes(state, &url).await?;
    Ok((
        bytes,
        Some(FallbackMeta {
            name: pack.name.clone(),
            version: version.to_string(),
            game_version: pack.mc_version.clone(),
        }),
    ))
}

// -------------------------------------------------------------- xml parsing
//
// The catalog is flat `<modpacks><modpack attr="…"/>…</modpacks>`; a small
// tolerant scanner beats pulling in an XML tree parser for attribute soup.

/// Scan every `<modpack …>` open tag in the catalog XML.
fn parse_modpacks(xml: &str) -> Vec<FtbPack> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(pos) = rest.find("<modpack") {
        let after = &rest[pos + "<modpack".len()..];
        // Distinguish `<modpack ` from the `<modpacks>` root element.
        let next = after.chars().next().unwrap_or(' ');
        if !(next.is_whitespace() || next == '/' || next == '>') {
            rest = after;
            continue;
        }
        // End of the open tag: the first '>' outside quoted attributes.
        let mut quoted = false;
        let mut end = None;
        for (i, c) in after.char_indices() {
            match c {
                '"' => quoted = !quoted,
                '>' if !quoted => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let attrs = parse_attrs(&after[..end]);
        let get = |k: &str| {
            unescape_entities(
                attrs
                    .iter()
                    .find(|(n, _)| n == k)
                    .map(|(_, v)| v.as_str())
                    .unwrap_or_default(),
            )
        };
        out.push(FtbPack {
            name: get("name"),
            author: get("author"),
            dir: get("dir"),
            file: get("url"),
            version: get("version"),
            old_versions: get("oldVersions")
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            mc_version: get("mcVersion"),
            logo: get("logo"),
            description: clean_text(&get("description")),
        });
        rest = &after[end + 1..];
    }
    out
}

/// Parse `key="value"` pairs out of an element's open-tag body. Quoted
/// values are consumed whole, so `=`/`>` inside descriptions are safe.
fn parse_attrs(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut it = tag.char_indices().peekable();
    while let Some((_, c)) = it.next() {
        if !(c.is_alphabetic() || c == '_') {
            continue;
        }
        let mut key = c.to_string();
        while let Some(&(_, c2)) = it.peek() {
            if c2 == '=' || c2.is_whitespace() {
                break;
            }
            key.push(c2);
            it.next();
        }
        while it.peek().is_some_and(|&(_, c2)| c2.is_whitespace()) {
            it.next();
        }
        if !it.peek().is_some_and(|&(_, c2)| c2 == '=') {
            continue; // bare word, not an attribute
        }
        it.next();
        while it.peek().is_some_and(|&(_, c2)| c2.is_whitespace()) {
            it.next();
        }
        let quote = match it.peek().map(|&(_, c2)| c2) {
            Some(q @ ('"' | '\'')) => {
                it.next();
                q
            }
            _ => continue, // unquoted values do not occur in this catalog
        };
        let mut val = String::new();
        for (_, c2) in it.by_ref() {
            if c2 == quote {
                break;
            }
            val.push(c2);
        }
        out.push((key, val));
    }
    out
}

/// Undo the catalog's XML entities (order matters: `&amp;` last).
fn unescape_entities(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Descriptions are old HTML blurbs: drop tags, collapse whitespace.
/// A `>` outside a tag (legal in XML attributes) stays as plain text.
fn clean_text(raw: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut last_ws = true;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if in_tag => {}
            c if c.is_whitespace() => {
                if !last_ws {
                    out.push(' ');
                    last_ws = true;
                }
            }
            c => {
                out.push(c);
                last_ws = false;
            }
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_catalog_entry() {
        let xml = r#"<?xml version="1.0"?>
<modpacks>
    <modpack author="The FTB Team" description="A &lt;b&gt;test&lt;/b&gt; pack &amp; more"
        dir="FTBTest" logo="FTBTestIcon.png" mcVersion="1.12.2" name="FTB Test"
        oldVersions="1.1.0;1.0.0" url="FTBTest.zip" version="1.1.0" />
</modpacks>"#;
        let packs = parse_modpacks(xml);
        assert_eq!(packs.len(), 1);
        let p = &packs[0];
        assert_eq!(p.dir, "FTBTest");
        assert_eq!(p.name, "FTB Test");
        assert_eq!(p.file, "FTBTest.zip");
        assert_eq!(p.version, "1.1.0");
        assert_eq!(p.old_versions, ["1.1.0", "1.0.0"]);
        assert_eq!(p.mc_version, "1.12.2");
        assert_eq!(p.description, "A test pack & more");
        assert_eq!(all_versions(p), ["1.1.0", "1.0.0"]);
    }

    #[test]
    fn quoted_gt_does_not_end_tag() {
        let xml = "<modpacks><modpack name=\"A > B\" description=\"x > y\" dir=\"D\" url=\"D.zip\" version=\"1\" oldVersions=\"\" mcVersion=\"1.16\"/></modpacks>";
        let packs = parse_modpacks(xml);
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].name, "A > B");
        assert_eq!(packs[0].description, "x > y");
    }
}
