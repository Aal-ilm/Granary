//! Go module paths, versions and hashes, following `golang.org/x/mod`
//! (`module`, `semver` and `sumdb/dirhash`), so Granary accepts exactly what
//! the go command sends and computes the same `h1:` hashes.

use std::io::Read;

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

/// The largest module zip, and the most its files may hold uncompressed.
pub const MAX_ZIP: u64 = 500 << 20;
/// The largest go.mod (and LICENSE) file.
pub const MAX_GO_MOD: u64 = 16 << 20;

const BAD_WINDOWS_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn mod_path_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~')
}

fn first_path_ok(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.')
}

fn check_elem(elem: &str) -> Result<(), String> {
    if elem.is_empty() {
        return Err("empty path element".into());
    }
    if elem.chars().all(|c| c == '.') {
        return Err(format!("invalid path element {elem:?}"));
    }
    if elem.starts_with('.') {
        return Err("leading dot in path element".into());
    }
    if elem.ends_with('.') {
        return Err("trailing dot in path element".into());
    }
    if let Some(c) = elem.chars().find(|c| !mod_path_ok(*c)) {
        return Err(format!("invalid char {c:?}"));
    }
    let short = elem.split('.').next().unwrap_or(elem);
    if BAD_WINDOWS_NAMES
        .iter()
        .any(|b| b.eq_ignore_ascii_case(short))
    {
        return Err(format!(
            "{short:?} disallowed as path element component on Windows"
        ));
    }
    if let Some(t) = short.rfind('~')
        && t < short.len() - 1
        && short[t + 1..].bytes().all(|b| b.is_ascii_digit())
    {
        return Err("trailing tilde and digits in path element".into());
    }
    Ok(())
}

/// `module.CheckPath`: whether `path` is a valid module path.
pub fn check_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty module path".into());
    }
    if path.starts_with('-') {
        return Err("leading dash".into());
    }
    if path.contains("//") {
        return Err("double slash".into());
    }
    if path.ends_with('/') {
        return Err("trailing slash".into());
    }
    for elem in path.split('/') {
        check_elem(elem)?;
    }
    let first = path.split('/').next().unwrap_or("");
    if !first.contains('.') {
        return Err("missing dot in first path element".into());
    }
    if let Some(c) = first.chars().find(|c| !first_path_ok(*c)) {
        return Err(format!("invalid char {c:?} in first path element"));
    }
    if split_path_version(path).is_none() {
        return Err("invalid version".into());
    }
    Ok(())
}

/// `module.SplitPathVersion`: the path without its major version suffix,
/// and the suffix (`/v2`, `.v1` for gopkg.in, or empty).
pub fn split_path_version(path: &str) -> Option<(&str, &str)> {
    if path.starts_with("gopkg.in/") {
        return split_gopkg_in(path);
    }
    let b = path.as_bytes();
    let mut i = b.len();
    let mut dot = false;
    while i > 0 && (b[i - 1].is_ascii_digit() || b[i - 1] == b'.') {
        if b[i - 1] == b'.' {
            dot = true;
        }
        i -= 1;
    }
    if i <= 1 || i == b.len() || b[i - 1] != b'v' || b[i - 2] != b'/' {
        return Some((path, ""));
    }
    let (prefix, major) = (&path[..i - 2], &path[i - 2..]);
    if dot || major.len() <= 2 || major.as_bytes()[2] == b'0' || major == "/v1" {
        return None;
    }
    Some((prefix, major))
}

fn split_gopkg_in(path: &str) -> Option<(&str, &str)> {
    let b = path.as_bytes();
    let mut i = b.len();
    if path.ends_with("-unstable") {
        i -= "-unstable".len();
    }
    while i > 0 && b[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i <= 1 || b[i - 1] != b'v' || b[i - 2] != b'.' {
        return None;
    }
    let (prefix, major) = (&path[..i - 2], &path[i - 2..]);
    if major.len() <= 2 || (major.as_bytes()[2] == b'0' && major != ".v0") {
        return None;
    }
    Some((prefix, major))
}

/// A parsed semantic version, as `golang.org/x/mod/semver` reads it.
struct Semver<'a> {
    major: &'a str,
    short: bool,
    build: &'a str,
}

fn parse_int(v: &str) -> Option<(&str, &str)> {
    let n = v.bytes().take_while(u8::is_ascii_digit).count();
    if n == 0 || (v.starts_with('0') && n != 1) {
        return None;
    }
    Some((&v[..n], &v[n..]))
}

fn ident_list(v: &str, numeric_check: bool) -> bool {
    !v.is_empty()
        && v.split('.').all(|part| {
            !part.is_empty()
                && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                && !(numeric_check
                    && part.len() > 1
                    && part.starts_with('0')
                    && part.bytes().all(|c| c.is_ascii_digit()))
        })
}

fn parse_semver(v: &str) -> Option<Semver<'_>> {
    let rest = v.strip_prefix('v')?;
    let (major, rest) = parse_int(rest)?;
    let Some(rest) = rest.strip_prefix('.') else {
        return rest.is_empty().then_some(Semver {
            major,
            short: true,
            build: "",
        });
    };
    let (_, rest) = parse_int(rest)?;
    let Some(rest) = rest.strip_prefix('.') else {
        return rest.is_empty().then_some(Semver {
            major,
            short: true,
            build: "",
        });
    };
    let (_, mut rest) = parse_int(rest)?;
    if let Some(pre) = rest.strip_prefix('-') {
        let end = pre.find('+').unwrap_or(pre.len());
        if !ident_list(&pre[..end], true) {
            return None;
        }
        rest = &pre[end..];
    }
    let mut build = "";
    if let Some(b) = rest.strip_prefix('+') {
        if !ident_list(b, false) {
            return None;
        }
        build = rest;
        rest = "";
    }
    rest.is_empty().then_some(Semver {
        major,
        short: false,
        build,
    })
}

/// Whether `v` is a canonical module version: `vMAJOR.MINOR.PATCH`, with an
/// optional pre-release, and `+incompatible` as the only build suffix.
pub fn is_canonical(v: &str) -> bool {
    parse_semver(v).is_some_and(|s| !s.short && (s.build.is_empty() || s.build == "+incompatible"))
}

/// `module.CheckPathMajor`: whether version `v` fits the path's major
/// version suffix.
pub fn check_path_major(v: &str, path_major: &str) -> Result<(), String> {
    let path_major = if path_major.starts_with(".v") {
        path_major.trim_end_matches("-unstable")
    } else {
        path_major
    };
    if v.starts_with("v0.0.0-") && path_major == ".v1" {
        return Ok(());
    }
    let Some(s) = parse_semver(v) else {
        return Err(format!("{v} is not a semantic version"));
    };
    let major = format!("v{}", s.major);
    let ok = if path_major.is_empty() {
        major == "v0" || major == "v1" || s.build == "+incompatible"
    } else {
        major == path_major[1..]
    };
    if ok {
        Ok(())
    } else {
        let want = if path_major.is_empty() {
            "v0 or v1"
        } else {
            &path_major[1..]
        };
        Err(format!("version {v} should be {want}, not {major}"))
    }
}

/// Check a module path and a canonical version together.
pub fn check(path: &str, version: &str) -> Result<(), String> {
    check_path(path)?;
    if !is_canonical(version) {
        return Err(format!("{version:?} is not a canonical version"));
    }
    let (_, major) = split_path_version(path).ok_or("invalid version suffix")?;
    check_path_major(version, major)
}

/// The case encoding of module paths and versions in URLs and file names:
/// every uppercase letter becomes `!` and its lowercase letter.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_uppercase() {
            out.push('!');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Undo [`escape`]. `None` for an uppercase letter, a `!` that isn't
/// followed by a lowercase letter, or a non-ASCII character.
pub fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut bang = false;
    for c in s.chars() {
        if !c.is_ascii() || c.is_ascii_uppercase() {
            return None;
        }
        if bang {
            if !c.is_ascii_lowercase() {
                return None;
            }
            out.push(c.to_ascii_uppercase());
            bang = false;
        } else if c == '!' {
            bang = true;
        } else {
            out.push(c);
        }
    }
    (!bang).then_some(out)
}

/// `dirhash.Hash1` over named files.
fn hash1<'a>(files: impl Iterator<Item = (&'a str, [u8; 32])>) -> String {
    let mut lines: Vec<(&str, [u8; 32])> = files.collect();
    lines.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = Sha256::new();
    for (name, sum) in lines {
        h.update(format!("{}  {name}\n", hex::encode(sum)));
    }
    format!("h1:{}", STANDARD.encode(h.finalize()))
}

/// The `h1:` hash of a go.mod file, as `go.sum` lists it.
pub fn go_mod_hash(data: &[u8]) -> String {
    hash1(std::iter::once(("go.mod", Sha256::digest(data).into())))
}

/// The `h1:` hash of a module zip (blocking), after checking that it is one
/// for `module@version`: every entry under `module@version/`, within the
/// size limits, no newline in a name, and no two names that differ only in
/// case.
pub fn zip_hash(path: &std::path::Path, module: &str, version: &str) -> Result<String, String> {
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size > MAX_ZIP {
        return Err(format!("the module zip is larger than {MAX_ZIP} bytes"));
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a zip file: {e}"))?;
    let prefix = format!("{module}@{version}/");
    let mut sums: Vec<(String, [u8; 32])> = Vec::with_capacity(zip.len());
    let mut folded = std::collections::HashSet::new();
    let mut total = 0u64;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.to_string();
        if !name.starts_with(&prefix) || name.contains('\n') {
            return Err(format!("unexpected file {name:?} in the module zip"));
        }
        if !folded.insert(name.to_lowercase()) {
            return Err(format!(
                "{name:?} appears twice in the module zip (ignoring case)"
            ));
        }
        let limit = if name == format!("{prefix}go.mod") || name == format!("{prefix}LICENSE") {
            MAX_GO_MOD
        } else {
            MAX_ZIP
        };
        let mut h = Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        let mut read = 0u64;
        loop {
            let n = entry.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            read += n as u64;
            total += n as u64;
            if read > limit || total > MAX_ZIP {
                return Err("the module zip's files are too large".into());
            }
            h.update(&buf[..n]);
        }
        sums.push((name, h.finalize().into()));
    }
    Ok(hash1(sums.iter().map(|(n, s)| (n.as_str(), *s))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in [
            "golang.org/x/text",
            "github.com/BurntSushi/toml",
            "example.com/mod/v2",
            "gopkg.in/yaml.v3",
            "gopkg.in/check.v1",
            "example.com/a~b",
        ] {
            assert!(check_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "golang",
            "Golang.org/x",
            "-x.org/a",
            "x.org//a",
            "x.org/a/",
            "x.org/.a",
            "x.org/a.",
            "x.org/a b",
            "x.org/con",
            "x.org/aux.txt",
            "x.org/abc~1",
            "x.org/mod/v1",
            "x.org/mod/v02",
            "gopkg.in/yaml",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn versions() {
        for ok in [
            "v1.2.3",
            "v0.0.0-20191109021931-daa7c04131f5",
            "v1.2.4-0.20191109021931-daa7c04131f5",
            "v2.0.0+incompatible",
            "v1.0.0-rc.1",
        ] {
            assert!(is_canonical(ok), "{ok}");
        }
        for bad in [
            "v1",
            "v1.2",
            "1.2.3",
            "v1.2.3+meta",
            "v01.2.3",
            "v1.2.3-01",
            "master",
        ] {
            assert!(!is_canonical(bad), "{bad}");
        }
        assert!(check("example.com/m", "v1.0.0").is_ok());
        assert!(check("example.com/m", "v2.0.0").is_err());
        assert!(check("example.com/m", "v2.0.0+incompatible").is_ok());
        assert!(check("example.com/m/v2", "v2.1.0").is_ok());
        assert!(check("example.com/m/v2", "v1.1.0").is_err());
        assert!(check("gopkg.in/yaml.v3", "v3.0.1").is_ok());
        assert!(check("gopkg.in/yaml.v3", "v2.0.0").is_err());
    }

    #[test]
    fn case_encoding() {
        assert_eq!(
            escape("github.com/BurntSushi/toml"),
            "github.com/!burnt!sushi/toml"
        );
        assert_eq!(
            unescape("github.com/!burnt!sushi/toml").as_deref(),
            Some("github.com/BurntSushi/toml")
        );
        for bad in ["github.com/Burnt", "x!", "x!!a", "x!1", "\u{e9}"] {
            assert_eq!(unescape(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn hashes_match_the_go_command() {
        // What `go mod download -json` reports for these.
        assert_eq!(
            go_mod_hash(b"module golang.org/x/text\n"),
            "h1:NqM8EUOU14njkJ3fqMW+pc6Ldnwhi/IjpwHt7yyuwOQ="
        );
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tests/fixtures/go/");
        let m = std::fs::read(format!("{fixtures}rsc.io-quote-v1.5.2.mod")).unwrap();
        assert_eq!(
            go_mod_hash(&m),
            "h1:LzX7hefJvL54yjefDEDHNONDjII0t9xZLPXsUe+TKr0="
        );
        let zip = std::path::PathBuf::from(format!("{fixtures}rsc.io-quote-v1.5.2.zip"));
        assert_eq!(
            zip_hash(&zip, "rsc.io/quote", "v1.5.2").unwrap(),
            "h1:w5fcysjrx7yqtD/aO+QwRjYZOKnaM9Uh2b40tElTs3Y="
        );
        // Checked against the module it claims to be.
        assert!(zip_hash(&zip, "rsc.io/quote", "v1.5.3").is_err());
    }
}
