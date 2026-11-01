//! go.mod, go.sum and go.work files from repos and pasted text, and Go
//! template lines (`module@version`).
//!
//! Everything here reads untrusted text. Sizes are checked before anything
//! is allocated, the lexer is a single pass over each line (no regex, no
//! recursion), blocks nest one level only, and every module path and
//! version goes through [`gomod::check_path`] and [`gomod::check`].

use std::borrow::Cow;

use base64::{Engine, engine::general_purpose::STANDARD};

use crate::gomod;

// Caps on untrusted files. Without data: revisit once Go repo templates
// have real use (docs/specs/go-spec.md, phase 3).
/// Largest go.mod or go.work read.
pub const MAX_MOD_BYTES: usize = 2 * 1024 * 1024;
/// Largest go.sum read.
pub const MAX_SUM_BYTES: usize = 16 * 1024 * 1024;
/// Longest line in any of the three files.
pub const MAX_LINE: usize = 4096;
/// Most `require` (and, separately, `replace`) entries in one go.mod.
pub const MAX_REQUIRES: usize = 5000;
/// Most lines in one go.sum.
pub const MAX_SUM_LINES: usize = 20_000;
/// Most `use` entries in one go.work.
pub const MAX_USES: usize = 100;
/// Most Go modules (go.mod files) one repo template reads.
pub const MAX_MODULES: usize = 200;
/// Longest Go template line, like every other template line.
pub const MAX_GO_LINE: usize = 500;

// ------------------------------------------------------------------ lexer

#[derive(Debug, PartialEq, Eq)]
enum Tok<'a> {
    Word(Cow<'a, str>),
    Open,
    Close,
}

/// One line's tokens and its trailing `//` comment.
struct Line<'a> {
    no: usize,
    toks: Vec<Tok<'a>>,
    comment: Option<&'a str>,
}

/// Check the whole text before any parsing: its size, and no NUL or
/// carriage return anywhere.
fn precheck(text: &str, max: usize, what: &str) -> Result<(), String> {
    if text.len() > max {
        return Err(format!("the {what} is larger than {} MB", max >> 20));
    }
    if text.as_bytes().contains(&0) {
        return Err(format!("the {what} contains a NUL byte"));
    }
    if text.as_bytes().contains(&b'\r') {
        return Err(format!(
            "the {what} contains a carriage return (Windows line endings are not read)"
        ));
    }
    Ok(())
}

/// Split one line (no newline in it) into tokens.
fn lex_line(no: usize, line: &str) -> Result<Line<'_>, String> {
    if line.len() > MAX_LINE {
        return Err(format!("line {no} is longer than {MAX_LINE} bytes"));
    }
    let b = line.as_bytes();
    let mut toks = Vec::new();
    let mut comment = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                comment = Some(line[i + 2..].trim());
                break;
            }
            b'(' => {
                toks.push(Tok::Open);
                i += 1;
            }
            b')' => {
                toks.push(Tok::Close);
                i += 1;
            }
            b'"' => {
                // Only \" and \\ are understood.
                let mut out = String::new();
                i += 1;
                loop {
                    match b.get(i) {
                        None => return Err(format!("line {no}: unterminated quoted string")),
                        Some(b'"') => break,
                        Some(b'\\') => match b.get(i + 1) {
                            Some(&e @ (b'"' | b'\\')) => {
                                out.push(e as char);
                                i += 2;
                            }
                            _ => return Err(format!("line {no}: unsupported escape in a string")),
                        },
                        Some(&x) if x < 0x20 || x == 0x7f => {
                            return Err(format!("line {no}: control character in a string"));
                        }
                        Some(_) => {
                            // Copy one whole character.
                            let ch = line[i..].chars().next().unwrap_or('\u{fffd}');
                            out.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                i += 1;
                toks.push(Tok::Word(Cow::Owned(out)));
            }
            b'`' => {
                let start = i + 1;
                let Some(len) = b[start..].iter().position(|&x| x == b'`') else {
                    return Err(format!("line {no}: unterminated backquoted string"));
                };
                let s = &line[start..start + len];
                if s.bytes().any(|x| x < 0x20 || x == 0x7f) {
                    return Err(format!("line {no}: control character in a string"));
                }
                toks.push(Tok::Word(Cow::Borrowed(s)));
                i = start + len + 1;
            }
            b'[' | b']' | b'{' | b'}' | b',' => {
                return Err(format!("line {no}: unexpected {:?}", c as char));
            }
            x if x < 0x20 || x == 0x7f => {
                return Err(format!("line {no}: control character"));
            }
            _ => {
                let start = i;
                while i < b.len() {
                    let x = b[i];
                    if matches!(
                        x,
                        b' ' | b'\t' | b'(' | b')' | b'"' | b'`' | b'[' | b']' | b'{' | b'}' | b','
                    ) || x < 0x20
                        || x == 0x7f
                        || (x == b'/' && b.get(i + 1) == Some(&b'/'))
                    {
                        break;
                    }
                    if x == b'/' && b.get(i + 1) == Some(&b'*') {
                        return Err(format!("line {no}: use // comments, not /* */"));
                    }
                    i += 1;
                }
                toks.push(Tok::Word(Cow::Borrowed(&line[start..i])));
            }
        }
    }
    Ok(Line { no, toks, comment })
}

/// One directive or block entry: its verb, its arguments and comment.
struct Entry<'a> {
    no: usize,
    verb: String,
    args: Vec<Cow<'a, str>>,
    comment: Option<&'a str>,
}

/// Lex and group a whole file into entries. Blocks (`verb (` ... `)`)
/// nest one level; anything else with parentheses is an error.
fn entries(text: &str) -> Result<Vec<Entry<'_>>, String> {
    let mut out = Vec::new();
    let mut block: Option<(usize, String)> = None;
    for (i, raw) in text.split('\n').enumerate() {
        let line = lex_line(i + 1, raw)?;
        let no = line.no;
        if line.toks.is_empty() {
            continue;
        }
        let parens = line
            .toks
            .iter()
            .filter(|t| matches!(t, Tok::Open | Tok::Close))
            .count();
        match &block {
            None => {
                if parens == 0 {
                    let mut words = words(line.toks);
                    let verb = words.remove(0).into_owned();
                    out.push(Entry {
                        no,
                        verb,
                        args: words,
                        comment: line.comment,
                    });
                } else if parens == 1
                    && line.toks.len() == 2
                    && line.toks[1] == Tok::Open
                    && let Tok::Word(v) = &line.toks[0]
                {
                    block = Some((no, v.to_string()));
                } else if line.toks.first() == Some(&Tok::Close) {
                    return Err(format!("line {no}: ')' without a block to close"));
                } else {
                    return Err(format!("line {no}: unexpected parenthesis"));
                }
            }
            Some((_, verb)) => {
                if parens == 0 {
                    out.push(Entry {
                        no,
                        verb: verb.clone(),
                        args: words(line.toks),
                        comment: line.comment,
                    });
                } else if line.toks.len() == 1 && line.toks[0] == Tok::Close {
                    block = None;
                } else if line.toks.contains(&Tok::Open) {
                    return Err(format!("line {no}: blocks can't be nested"));
                } else {
                    return Err(format!("line {no}: unexpected parenthesis"));
                }
            }
        }
    }
    if let Some((no, verb)) = block {
        return Err(format!("line {no}: the {verb} block is never closed"));
    }
    Ok(out)
}

fn words(toks: Vec<Tok<'_>>) -> Vec<Cow<'_, str>> {
    toks.into_iter()
        .filter_map(|t| match t {
            Tok::Word(w) => Some(w),
            _ => None,
        })
        .collect()
}

// ----------------------------------------------------------------- go.mod

/// A `require` entry that passed [`gomod::check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Require {
    pub path: String,
    pub version: String,
    pub indirect: bool,
}

/// What a `replace` points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Another module version (checked).
    Module { path: String, version: String },
    /// A directory, as written.
    Dir(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replace {
    pub line: usize,
    pub old: String,
    pub old_version: Option<String>,
    pub new: Target,
}

/// A parsed go.mod. Entries that are well formed but not valid (a bad
/// module path, a non-canonical version) are left out and listed in
/// `skipped` with the reason.
#[derive(Clone, Debug, Default)]
pub struct GoMod {
    pub module: Option<String>,
    pub go: Option<String>,
    pub toolchain: Option<String>,
    pub requires: Vec<Require>,
    pub replaces: Vec<Replace>,
    pub skipped: Vec<String>,
}

/// Whether a replacement is a directory rather than a module path
/// (`modfile.IsDirectoryPath`, plus Windows forms, all counted as paths).
pub fn is_dir_path(s: &str) -> bool {
    s == "."
        || s == ".."
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with(".\\")
        || s.starts_with("..\\")
        || s.starts_with('\\')
        || s.as_bytes().get(1) == Some(&b':')
}

/// `target` taken relative to the repo directory `base` ("" for the
/// root), as a repo-relative path without `.` or `..`. `None` when it is
/// absolute, uses backslashes or a drive letter, or leaves the repo.
pub fn resolve_in_repo(base: &str, target: &str) -> Option<String> {
    if target.starts_with('/') || target.contains('\\') || target.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

fn simple_version(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+')
        })
}

/// Read a go.mod.
pub fn parse_mod(text: &str) -> Result<GoMod, String> {
    precheck(text, MAX_MOD_BYTES, "go.mod")?;
    let mut m = GoMod::default();
    let mut replaces = 0usize;
    for e in entries(text)? {
        let no = e.no;
        let args = &e.args;
        match e.verb.as_str() {
            "module" => {
                if m.module.is_some() {
                    return Err(format!("line {no}: module is given twice"));
                }
                match args.as_slice() {
                    [p] => m.module = Some(p.to_string()),
                    _ => return Err(format!("line {no}: usage: module path")),
                }
            }
            "go" | "toolchain" => match args.as_slice() {
                [v] if simple_version(v) => {
                    let slot = if e.verb == "go" {
                        &mut m.go
                    } else {
                        &mut m.toolchain
                    };
                    *slot = Some(v.to_string());
                }
                _ => m
                    .skipped
                    .push(format!("line {no}: {} is not understood", e.verb)),
            },
            "require" => {
                if m.requires.len() + 1 > MAX_REQUIRES {
                    return Err(format!("more than {MAX_REQUIRES} requirements"));
                }
                match args.as_slice() {
                    [p, v] => match gomod::check(p, v) {
                        Ok(()) => m.requires.push(Require {
                            path: p.to_string(),
                            version: v.to_string(),
                            indirect: e
                                .comment
                                .is_some_and(|c| c == "indirect" || c.starts_with("indirect;")),
                        }),
                        Err(why) => m.skipped.push(format!("{p}@{v}: {why}")),
                    },
                    _ => m
                        .skipped
                        .push(format!("line {no}: usage: require module/path v1.2.3")),
                }
            }
            "replace" => {
                replaces += 1;
                if replaces > MAX_REQUIRES {
                    return Err(format!("more than {MAX_REQUIRES} replacements"));
                }
                match replace(no, args) {
                    Ok(r) => m.replaces.push(r),
                    Err(why) => m.skipped.push(why),
                }
            }
            "exclude" | "retract" | "godebug" | "ignore" | "tool" => {}
            other => m
                .skipped
                .push(format!("line {no}: unknown directive {other:?}")),
        }
    }
    Ok(m)
}

/// `old [version] => new [version]`.
fn replace(no: usize, args: &[Cow<'_, str>]) -> Result<Replace, String> {
    let usage =
        || format!("line {no}: usage: replace module [version] => module version, or => ./dir");
    let arrow = args.iter().position(|a| a == "=>").ok_or_else(usage)?;
    let (left, right) = (&args[..arrow], &args[arrow + 1..]);
    let (old, old_version) = match left {
        [p] => (p, None),
        [p, v] => (p, Some(v)),
        _ => return Err(usage()),
    };
    gomod::check_path(old).map_err(|e| format!("replace {old}: {e}"))?;
    if let Some(v) = old_version {
        gomod::check(old, v).map_err(|e| format!("replace {old} {v}: {e}"))?;
    }
    let new = match right {
        [d] if is_dir_path(d) => Target::Dir(d.to_string()),
        [p, v] => {
            gomod::check(p, v).map_err(|e| format!("replace {old} => {p} {v}: {e}"))?;
            Target::Module {
                path: p.to_string(),
                version: v.to_string(),
            }
        }
        [p] => {
            return Err(format!(
                "replace {old} => {p}: a replacement module needs a version"
            ));
        }
        _ => return Err(usage()),
    };
    Ok(Replace {
        line: no,
        old: old.to_string(),
        old_version: old_version.map(|v| v.to_string()),
        new,
    })
}

impl GoMod {
    /// Template lines for the requirements, and what was left out. A
    /// requirement replaced by another module version becomes that
    /// version; one replaced by a directory is not a Go line.
    pub fn lines(&self) -> (Vec<String>, Vec<String>) {
        let mut lines = Vec::new();
        let mut skipped = self.skipped.clone();
        for r in &self.requires {
            let rep = self.replaces.iter().find(|x| {
                x.old == r.path && x.old_version.as_ref().is_none_or(|v| *v == r.version)
            });
            let line = match rep.map(|x| &x.new) {
                None => format!("{}@{}", r.path, r.version),
                Some(Target::Module { path, version }) => format!("{path}@{version}"),
                Some(Target::Dir(d)) => {
                    skipped.push(format!(
                        "{}: replaced by the directory {d:?}, which is not a module to fetch",
                        r.path
                    ));
                    continue;
                }
            };
            match check_line(&line) {
                Ok(_) => lines.push(line),
                Err(why) => skipped.push(format!("{line}: {why}")),
            }
        }
        (lines, skipped)
    }
}

// ----------------------------------------------------------------- go.sum

/// One valid go.sum line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SumLine {
    pub path: String,
    pub version: String,
    /// The line is for the version's go.mod (`v1.2.3/go.mod`).
    pub go_mod: bool,
    /// `h1:` and the base64 of exactly 32 bytes.
    pub hash: String,
}

/// A parsed go.sum: the valid lines, and the numbers of the others with
/// the reason.
#[derive(Clone, Debug, Default)]
pub struct GoSum {
    pub lines: Vec<SumLine>,
    pub invalid: Vec<(usize, String)>,
}

fn sum_line(line: &str) -> Result<SumLine, String> {
    let mut f = line.split(' ');
    let (Some(path), Some(ver), Some(hash), None) = (f.next(), f.next(), f.next(), f.next()) else {
        return Err("expected `module version h1:hash`".into());
    };
    let (version, go_mod) = match ver.strip_suffix("/go.mod") {
        Some(v) => (v, true),
        None => (ver, false),
    };
    gomod::check(path, version)?;
    let b64 = hash
        .strip_prefix("h1:")
        .ok_or("the hash is not an h1: hash")?;
    if b64.len() != 44 {
        return Err("the h1: hash is not 32 bytes".into());
    }
    let raw = STANDARD
        .decode(b64)
        .map_err(|_| "the h1: hash is not base64")?;
    if raw.len() != 32 || STANDARD.encode(&raw) != b64 {
        return Err("the h1: hash is not 32 bytes".into());
    }
    Ok(SumLine {
        path: path.into(),
        version: version.into(),
        go_mod,
        hash: hash.into(),
    })
}

/// Read a go.sum. Lines that aren't valid are listed, not fatal; the file
/// as a whole fails only on its size, line count or length, NUL or CR.
pub fn parse_sum(text: &str) -> Result<GoSum, String> {
    precheck(text, MAX_SUM_BYTES, "go.sum")?;
    let mut s = GoSum::default();
    let mut count = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if line.len() > MAX_LINE {
            return Err(format!("line {} is longer than {MAX_LINE} bytes", i + 1));
        }
        if line.is_empty() {
            continue;
        }
        count += 1;
        if count > MAX_SUM_LINES {
            return Err(format!("more than {MAX_SUM_LINES} lines"));
        }
        match sum_line(line) {
            Ok(l) => s.lines.push(l),
            Err(why) => s.invalid.push((i + 1, why)),
        }
    }
    Ok(s)
}

impl GoSum {
    /// The valid lines as a go.sum file.
    #[allow(dead_code)] // used by starters in the next change
    pub fn text(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            out.push_str(&l.path);
            out.push(' ');
            out.push_str(&l.version);
            if l.go_mod {
                out.push_str("/go.mod");
            }
            out.push(' ');
            out.push_str(&l.hash);
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------- go.work

/// A parsed go.work.
#[derive(Clone, Debug, Default)]
pub struct GoWork {
    pub go: Option<String>,
    /// The `use` directories, relative to the repo root.
    pub uses: Vec<String>,
    pub replaces: Vec<Replace>,
    pub skipped: Vec<String>,
}

/// Read a go.work found in the repo directory `dir` ("" for the root).
/// A `use` that is absolute or leaves the repo fails the whole file.
pub fn parse_work(text: &str, dir: &str) -> Result<GoWork, String> {
    precheck(text, MAX_MOD_BYTES, "go.work")?;
    let mut w = GoWork::default();
    let mut replaces = 0usize;
    for e in entries(text)? {
        let no = e.no;
        match (e.verb.as_str(), e.args.as_slice()) {
            ("use", [p]) => {
                if w.uses.len() + 1 > MAX_USES {
                    return Err(format!("more than {MAX_USES} use entries"));
                }
                let Some(rel) = resolve_in_repo(dir, p) else {
                    return Err(format!(
                        "line {no}: use {p:?} points outside the repo, so the workspace is not read"
                    ));
                };
                if !w.uses.contains(&rel) {
                    w.uses.push(rel);
                }
            }
            ("use", _) => return Err(format!("line {no}: usage: use ./dir")),
            ("go", [v]) if simple_version(v) => w.go = Some(v.to_string()),
            ("replace", args) => {
                replaces += 1;
                if replaces > MAX_REQUIRES {
                    return Err(format!("more than {MAX_REQUIRES} replacements"));
                }
                match replace(no, args) {
                    Ok(r) => w.replaces.push(r),
                    Err(why) => w.skipped.push(why),
                }
            }
            ("toolchain" | "godebug" | "go", _) => {}
            (other, _) => w
                .skipped
                .push(format!("line {no}: unknown directive {other:?}")),
        }
    }
    Ok(w)
}

// ----------------------------------------------------------- template line

/// Check one Go template line, `module@version` or `module@latest`;
/// returns the module path.
pub fn check_line(line: &str) -> Result<String, String> {
    const HINT: &str = "expected module@version, like golang.org/x/text@v0.21.0 (or @latest)";
    if line.len() > MAX_GO_LINE {
        return Err("line is too long".into());
    }
    if line
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == 0x7f)
    {
        return Err(HINT.into());
    }
    let (path, version) = line.split_once('@').ok_or(HINT)?;
    if version == "latest" {
        gomod::check_path(path).map_err(|e| format!("{path:?} is not a module path: {e}"))?;
        return Ok(path.to_string());
    }
    gomod::check_path(path).map_err(|e| format!("{path:?} is not a module path: {e}"))?;
    gomod::check(path, version).map_err(|e| format!("{path}@{version}: {e}"))?;
    if let Some(core) = version.strip_suffix("+incompatible") {
        // Only for v2 and later of a path without a major version suffix.
        let (_, suffix) = gomod::split_path_version(path).unwrap_or((path, ""));
        let major = core
            .strip_prefix('v')
            .and_then(|r| r.split(['.', '-']).next())
            .and_then(|m| m.parse::<u64>().ok())
            .unwrap_or(0);
        if !suffix.is_empty() || major < 2 {
            return Err(format!(
                "{version} is not allowed here: +incompatible is only for v2 and later of a module without a /vN suffix"
            ));
        }
    }
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "h1:47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

    #[test]
    fn reads_requires_blocks_comments_and_quotes() {
        let m = parse_mod(
            "// a module\nmodule example.com/app\n\ngo 1.22\ntoolchain go1.22.4\n\nrequire golang.org/x/text v0.21.0\nrequire (\n\t\"github.com/BurntSushi/toml\" v1.4.0 // indirect\n\t`gopkg.in/yaml.v3` v3.0.1\n\texample.com/bad v1\n)\nexclude example.com/x v1.0.0\n",
        )
        .unwrap();
        assert_eq!(m.module.as_deref(), Some("example.com/app"));
        assert_eq!(m.go.as_deref(), Some("1.22"));
        assert_eq!(m.toolchain.as_deref(), Some("go1.22.4"));
        let r: Vec<(&str, &str, bool)> = m
            .requires
            .iter()
            .map(|r| (r.path.as_str(), r.version.as_str(), r.indirect))
            .collect();
        assert_eq!(
            r,
            vec![
                ("golang.org/x/text", "v0.21.0", false),
                ("github.com/BurntSushi/toml", "v1.4.0", true),
                ("gopkg.in/yaml.v3", "v3.0.1", false),
            ]
        );
        assert_eq!(m.skipped.len(), 1, "{:?}", m.skipped);
        let (lines, _) = m.lines();
        assert_eq!(lines[1], "github.com/BurntSushi/toml@v1.4.0");
    }

    #[test]
    fn replaces_become_lines_or_are_left_out() {
        let m = parse_mod(
            "module m\nrequire (\n\ta.example/x v1.0.0\n\tb.example/y v1.0.0\n\tc.example/z v1.0.0\n)\nreplace a.example/x => fork.example/x v1.0.1\nreplace b.example/y v1.0.0 => ./local\nreplace c.example/z => d.example/z\n",
        )
        .unwrap();
        let (lines, skipped) = m.lines();
        assert_eq!(lines, vec!["fork.example/x@v1.0.1", "c.example/z@v1.0.0"]);
        assert!(skipped.iter().any(|s| s.contains("needs a version")));
        assert!(skipped.iter().any(|s| s.contains("\"./local\"")));
        assert_eq!(m.replaces[1].new, Target::Dir("./local".into()));
    }

    #[test]
    fn refuses_bad_structure() {
        for bad in [
            "require (\nx.example/a v1.0.0\n",
            "require x.example/a v1.0.0\n)\n",
            "require (\nreplace (\n)\n)\n",
            "require (x.example/a v1.0.0)\n",
            "require \"x.example/a\\n\" v1.0.0\n",
            "require \"x.example/a v1.0.0\n",
            "require `x.example/a v1.0.0\n",
            "module a\x00\n",
            "module a\r\n",
            "module a /* no */\n",
            "require [x] v1\n",
            "module a\nmodule b\n",
        ] {
            assert!(parse_mod(bad).is_err(), "{bad:?}");
        }
        // Escaped quote and backslash are fine.
        let m = parse_mod("module \"a\\\"b\\\\\"\n").unwrap();
        assert_eq!(m.module.as_deref(), Some("a\"b\\"));
    }

    #[test]
    fn caps_accept_n_and_refuse_n_plus_one() {
        let reqs = |n: usize| {
            let mut s = String::from("require (\n");
            for i in 0..n {
                s.push_str(&format!("\tx.example/m{i} v1.0.0\n"));
            }
            s.push_str(")\n");
            s
        };
        assert_eq!(
            parse_mod(&reqs(MAX_REQUIRES)).unwrap().requires.len(),
            MAX_REQUIRES
        );
        assert!(parse_mod(&reqs(MAX_REQUIRES + 1)).is_err());

        let line = |n: usize| format!("module {}\n", "a".repeat(n - "module ".len()));
        assert!(parse_mod(&line(MAX_LINE)).is_ok());
        assert!(parse_mod(&line(MAX_LINE + 1)).is_err());

        let size = |n: usize| "\n".repeat(n);
        assert!(parse_mod(&size(MAX_MOD_BYTES)).is_ok());
        assert!(parse_mod(&size(MAX_MOD_BYTES + 1)).is_err());
        assert!(parse_sum(&size(MAX_SUM_BYTES)).is_ok());
        assert!(parse_sum(&size(MAX_SUM_BYTES + 1)).is_err());

        let sums = |n: usize| format!("x.example/a v1.0.0 {H}\n").repeat(n);
        assert_eq!(
            parse_sum(&sums(MAX_SUM_LINES)).unwrap().lines.len(),
            MAX_SUM_LINES
        );
        assert!(parse_sum(&sums(MAX_SUM_LINES + 1)).is_err());

        let uses = |n: usize| {
            let mut s = String::from("go 1.22\nuse (\n");
            for i in 0..n {
                s.push_str(&format!("\t./m{i}\n"));
            }
            s.push_str(")\n");
            s
        };
        assert_eq!(
            parse_work(&uses(MAX_USES), "").unwrap().uses.len(),
            MAX_USES
        );
        assert!(parse_work(&uses(MAX_USES + 1), "").is_err());
    }

    #[test]
    fn reads_go_sum_lines() {
        let short = STANDARD.encode([0u8; 31]);
        let long = STANDARD.encode([0u8; 33]);
        let text = format!(
            "golang.org/x/text v0.21.0 {H}\ngolang.org/x/text v0.21.0/go.mod {H}\n\nx.example/a v1.0.0 h1:{short}\nx.example/a v1.0.0 h1:{long}\nx.example/a v1 {H}\nx.example/a v1.0.0  {H}\nx.example/a v1.0.0 h2:abc\n"
        );
        let s = parse_sum(&text).unwrap();
        assert_eq!(s.lines.len(), 2);
        assert!(s.lines[1].go_mod);
        assert_eq!(
            s.invalid.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![4, 5, 6, 7, 8]
        );
        assert_eq!(parse_sum(&s.text()).unwrap().lines, s.lines);
        assert!(parse_sum("a\x00").is_err());
        assert!(parse_sum("a\r\n").is_err());
    }

    #[test]
    fn reads_go_work_uses_inside_the_repo() {
        let w = parse_work(
            "go 1.22\nuse ./a\nuse (\n\t.\n\t./b/c\n\t../sibling\n)\n",
            "sub",
        )
        .unwrap();
        assert_eq!(w.uses, vec!["sub/a", "sub", "sub/b/c", "sibling"]);
        for bad in [
            "use ../x\n",
            "use /abs\n",
            "use ./a/../../x\n",
            "use C:\\x\n",
            "use (\n",
        ] {
            assert!(parse_work(bad, "").is_err(), "{bad:?}");
        }
        assert_eq!(resolve_in_repo("", "./a/./b/"), Some("a/b".into()));
        assert_eq!(resolve_in_repo("a", "../.."), None);
    }

    #[test]
    fn go_lines() {
        assert_eq!(
            check_line("golang.org/x/text@v0.21.0").unwrap(),
            "golang.org/x/text"
        );
        assert!(check_line("golang.org/x/text@latest").is_ok());
        assert!(check_line("github.com/BurntSushi/toml@v1.4.0").is_ok());
        assert!(check_line("github.com/a/b@v2.0.0+incompatible").is_ok());
        for bad in [
            "Golang.org/x/text@v0.21.0",
            "github.com/!burnt!sushi/toml@v1.4.0",
            "golang.org/x/../text@v0.21.0",
            "golang.org/x/text@v0.21.0\nexample.com/evil@v1.0.0",
            "golang.org/x/text@v0.21",
            "golang.org/x/text@master",
            "golang.org/x/text",
            "golang.org/x/text@v0.21.0+incompatible",
            "github.com/a/b/v2@v2.0.0+incompatible",
            "golang.org/x/text @v0.21.0",
        ] {
            assert!(check_line(bad).is_err(), "{bad:?}");
        }
        let long = format!("example.com/{}@v1.0.0", "a".repeat(MAX_GO_LINE));
        assert!(check_line(&long).unwrap_err().contains("too long"));
    }
}
