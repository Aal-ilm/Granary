//! Authenticating checksum database answers, as the go command does
//! (`golang.org/x/mod/sumdb`): a lookup's go.sum lines are trusted only
//! after its signed tree note verifies against the database's public key
//! *and* the record is proved to be in that tree, by recomputing the tree
//! hash from the log's tiles. A valid signature alone proves nothing about
//! the record next to it.

use std::{collections::HashMap, future::Future};

use base64::{Engine, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

/// The tile height sum.golang.org uses, and the go command assumes.
pub const TILE_HEIGHT: u32 = 8;

/// sum.golang.org's public key, as the go command knows it.
pub const SUM_GOLANG_ORG_KEY: &str =
    "sum.golang.org+033de0ae+Ac4zctda0e5eza+HJyk9SxEdh+s3Ux18htTTAD8OuAn8";

// ------------------------------------------------------------- signed notes

/// A note verifier key: `<name>+<hash>+<base64 of 0x01 and the Ed25519 key>`.
#[derive(Clone, Debug)]
pub struct Verifier {
    pub name: String,
    hash: u32,
    key: [u8; 32],
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(char::is_whitespace) && !name.contains('+')
}

fn key_hash(name: &str, key: &[u8]) -> u32 {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update(b"\n");
    h.update(key);
    let sum = h.finalize();
    u32::from_be_bytes([sum[0], sum[1], sum[2], sum[3]])
}

impl Verifier {
    pub fn parse(vkey: &str) -> Result<Self, String> {
        let bad = || format!("{vkey:?} is not a checksum database key (name+hash+key)");
        let (name, rest) = vkey.split_once('+').ok_or_else(bad)?;
        let (hash16, key64) = rest.split_once('+').ok_or_else(bad)?;
        let hash = u32::from_str_radix(hash16, 16).map_err(|_| bad())?;
        let key = STANDARD.decode(key64).map_err(|_| bad())?;
        if hash16.len() != 8 || !valid_name(name) || key.is_empty() {
            return Err(bad());
        }
        if key_hash(name, &key) != hash {
            return Err(format!("{vkey:?}: the key's hash doesn't match the key"));
        }
        // Algorithm 1 is Ed25519, the only one there is.
        if key[0] != 1 || key.len() != 33 {
            return Err(format!("{vkey:?}: not an Ed25519 key"));
        }
        let mut k = [0u8; 32];
        k.copy_from_slice(&key[1..]);
        Ok(Self {
            name: name.to_string(),
            hash,
            key: k,
        })
    }

    fn verify(&self, msg: &[u8], sig: &[u8]) -> bool {
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, self.key)
            .verify(msg, sig)
            .is_ok()
    }
}

/// A signature line starts with an em dash (U+2014) and a space.
const SIG_PREFIX: &str = "\u{2014} ";

/// `note.Open`: the note's text, if it carries a valid signature by `v`.
pub fn open_note<'a>(msg: &'a [u8], v: &Verifier) -> Result<&'a [u8], String> {
    let text = std::str::from_utf8(msg).map_err(|_| "the signed note isn't UTF-8")?;
    if text.chars().any(|c| (c as u32) < 0x20 && c != '\n') {
        return Err("the signed note has control characters".into());
    }
    let split = text
        .rfind("\n\n")
        .ok_or("the signed note has no signatures")?;
    let (body, sigs) = (&text[..split + 1], &text[split + 2..]);
    if sigs.is_empty() || !sigs.ends_with('\n') {
        return Err("the signed note is malformed".into());
    }
    let mut verified = false;
    for (i, line) in sigs.lines().enumerate() {
        if i >= 100 {
            return Err("the signed note has too many signatures".into());
        }
        let line = line
            .strip_prefix(SIG_PREFIX)
            .ok_or("the signed note is malformed")?;
        let (name, b64) = line.split_once(' ').ok_or("the signed note is malformed")?;
        let sig = STANDARD
            .decode(b64)
            .map_err(|_| "the signed note is malformed")?;
        if !valid_name(name) || sig.len() < 5 {
            return Err("the signed note is malformed".into());
        }
        let hash = u32::from_be_bytes([sig[0], sig[1], sig[2], sig[3]]);
        if name != v.name || hash != v.hash {
            continue;
        }
        if !v.verify(body.as_bytes(), &sig[4..]) {
            return Err(format!("the note's signature by {name} is invalid"));
        }
        verified = true;
    }
    if !verified {
        return Err(format!("the note isn't signed by {}", v.name));
    }
    Ok(body.as_bytes())
}

/// A tree head: its size and root hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
    pub n: i64,
    pub hash: Hash,
}

/// `tlog.ParseTree`: the tree a note's text describes.
pub fn parse_tree(text: &[u8]) -> Result<Tree, String> {
    let bad = || "malformed tree note".to_string();
    let text = std::str::from_utf8(text).map_err(|_| bad())?;
    let rest = text
        .strip_prefix("go.sum database tree\n")
        .ok_or_else(bad)?;
    let mut lines = rest.splitn(3, '\n');
    let (Some(n), Some(h), Some(_)) = (lines.next(), lines.next(), lines.next()) else {
        return Err(bad());
    };
    let size: i64 = n.parse().map_err(|_| bad())?;
    if size < 0 || n != size.to_string() {
        return Err(bad());
    }
    let h = STANDARD.decode(h).map_err(|_| bad())?;
    let hash: Hash = h.try_into().map_err(|_| bad())?;
    Ok(Tree { n: size, hash })
}

/// `tlog.ParseRecord`: a lookup answer's record id, record text, and the
/// signed tree note after it.
pub fn parse_record(msg: &[u8]) -> Result<(i64, &[u8], &[u8]), String> {
    let bad = || "malformed record data".to_string();
    let nl = msg.iter().position(|b| *b == b'\n').ok_or_else(bad)?;
    let id: i64 = std::str::from_utf8(&msg[..nl])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(bad)?;
    let rest = &msg[nl + 1..];
    let end = rest.windows(2).position(|w| w == b"\n\n").ok_or_else(bad)?;
    let (text, note) = (&rest[..end + 1], &rest[end + 2..]);
    let valid = std::str::from_utf8(text).is_ok_and(|t| {
        t.ends_with('\n')
            && !t.contains("\n\n")
            && !t.chars().any(|c| (c as u32) < 0x20 && c != '\n')
    });
    if !valid {
        return Err(bad());
    }
    Ok((id, text, note))
}

// ------------------------------------------------------------------- tlog

pub fn record_hash(data: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(data);
    h.finalize().into()
}

pub fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// `tlog.StoredHashIndex`.
pub fn stored_hash_index(level: u32, n: i64) -> i64 {
    let mut n = n;
    for _ in 0..level {
        n = 2 * n + 1;
    }
    let mut i = 0i64;
    let mut m = n;
    while m > 0 {
        i += m;
        m >>= 1;
    }
    i + level as i64
}

/// `tlog.SplitStoredHashIndex`.
pub fn split_stored_hash_index(index: i64) -> (u32, i64) {
    let mut n = index / 2;
    let mut index_n = stored_hash_index(0, n);
    loop {
        let x = index_n + 1 + (n + 1).trailing_zeros() as i64;
        if x > index {
            break;
        }
        n += 1;
        index_n = x;
    }
    let level = (index - index_n) as u32;
    (level, n >> level)
}

/// A log tile: height, level, index, and width (hashes it holds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Tile {
    pub h: u32,
    pub l: u32,
    pub n: i64,
    pub w: i64,
}

impl Tile {
    /// `Tile.Path`, without the database name: `tile/8/0/x001/234.p/5`.
    pub fn path(&self) -> String {
        let mut n = self.n;
        let mut s = format!("{:03}", n % 1000);
        while n >= 1000 {
            n /= 1000;
            s = format!("x{:03}/{s}", n % 1000);
        }
        let p = if self.w != 1 << self.h {
            format!(".p/{}", self.w)
        } else {
            String::new()
        };
        format!("tile/{}/{}/{s}{p}", self.h, self.l)
    }
}

/// `tlog.tileForIndex`: the tile holding a stored hash, and the byte range
/// of that hash's subtree in it.
fn tile_for_index(h: u32, index: i64) -> (Tile, usize, usize) {
    let (mut level, mut n) = split_stored_hash_index(index);
    let l = level / h;
    level -= l * h;
    let tn = (n << level) >> h;
    n -= (tn << h) >> level;
    let w = (n + 1) << level;
    (
        Tile { h, l, n: tn, w },
        ((n << level) as usize) * 32,
        (((n + 1) << level) as usize) * 32,
    )
}

fn tile_hash(data: &[u8]) -> Hash {
    if data.len() == 32 {
        return data.try_into().unwrap();
    }
    let half = data.len() / 2;
    node_hash(&tile_hash(&data[..half]), &tile_hash(&data[half..]))
}

/// `tlog.HashFromTile`.
fn hash_from_tile(t: &Tile, data: &[u8], index: i64) -> Result<Hash, String> {
    if t.h < 1 || t.h > 30 || t.l >= 64 || t.w < 1 || t.w > 1 << t.h {
        return Err(format!("invalid tile {}", t.path()));
    }
    if data.len() < (t.w as usize) * 32 {
        return Err(format!("tile {} is too short", t.path()));
    }
    let (t1, start, end) = tile_for_index(t.h, index);
    if t.l != t1.l || t.n != t1.n || t.w < t1.w {
        return Err(format!(
            "index {index} is in {} not {}",
            t1.path(),
            t.path()
        ));
    }
    Ok(tile_hash(&data[start..end]))
}

/// `tlog.tileParent`; `None` for the empty tile.
fn tile_parent(t: Tile, k: u32, n: i64) -> Option<Tile> {
    let mut t = t;
    t.l += k;
    t.n >>= k * t.h;
    t.w = 1 << t.h;
    let max = n >> (t.l * t.h);
    if (t.n << t.h) + t.w >= max {
        if (t.n << t.h) >= max {
            return None;
        }
        t.w = max - (t.n << t.h);
    }
    Some(t)
}

fn maxpow2(n: i64) -> (i64, u32) {
    let mut l = 0u32;
    while (1i64 << (l + 1)) < n {
        l += 1;
    }
    (1 << l, l)
}

/// `tlog.subTreeIndex`: the stored hashes whose combination is the hash of
/// the tree of size `hi`.
fn sub_tree_index(mut lo: i64, hi: i64) -> Vec<i64> {
    let mut need = Vec::new();
    while lo < hi {
        let (k, level) = maxpow2(hi - lo + 1);
        need.push(stored_hash_index(level, lo >> level));
        lo += k;
    }
    need
}

/// `tlog.TileHashReader(tree).ReadHashes`: the stored hashes at `indexes`,
/// read from tiles that are first authenticated against `tree`'s hash.
/// `read_tiles` fetches tile data; it may come from a cache.
pub async fn read_hashes<F, Fut>(
    tree: Tree,
    indexes: &[i64],
    read_tiles: F,
) -> Result<(Vec<Hash>, Vec<(Tile, Vec<u8>)>), String>
where
    F: FnOnce(Vec<Tile>) -> Fut,
    Fut: Future<Output = Result<Vec<Vec<u8>>, String>>,
{
    let h = TILE_HEIGHT;
    let mut order: HashMap<Tile, usize> = HashMap::new();
    let mut tiles: Vec<Tile> = Vec::new();

    let stx = sub_tree_index(0, tree.n);
    let mut stx_order = Vec::with_capacity(stx.len());
    for x in &stx {
        let (t, _, _) = tile_for_index(h, *x);
        let t = tile_parent(t, 0, tree.n).ok_or("bad math reading tiles")?;
        let j = *order.entry(t).or_insert_with(|| {
            tiles.push(t);
            tiles.len() - 1
        });
        stx_order.push(j);
    }

    let mut index_order = vec![0usize; indexes.len()];
    for (i, x) in indexes.iter().enumerate() {
        if *x >= stored_hash_index(0, tree.n) {
            return Err("the record is not in the tree".into());
        }
        let (tile, _, _) = tile_for_index(h, *x);
        let mut k = 0u32;
        loop {
            let p = tile_parent(tile, k, tree.n).ok_or("bad math reading tiles")?;
            if let Some(j) = order.get(&p) {
                if k == 0 {
                    index_order[i] = *j;
                }
                break;
            }
            k += 1;
        }
        while k > 0 {
            k -= 1;
            let p = tile_parent(tile, k, tree.n).ok_or("bad math reading tiles")?;
            if p.w != 1 << p.h {
                return Err("bad math reading tiles".into());
            }
            order.insert(p, tiles.len());
            if k == 0 {
                index_order[i] = tiles.len();
            }
            tiles.push(p);
        }
    }

    let data = read_tiles(tiles.clone()).await?;
    if data.len() != tiles.len() {
        return Err("tile reader returned the wrong number of tiles".into());
    }
    for (t, d) in tiles.iter().zip(&data) {
        if d.len() != (t.w as usize) * 32 {
            return Err(format!("tile {} has the wrong size", t.path()));
        }
    }

    // The tiles must reproduce the signed tree hash.
    let mut th: Option<Hash> = None;
    for i in (0..stx.len()).rev() {
        let j = stx_order[i];
        let hx = hash_from_tile(&tiles[j], &data[j], stx[i])?;
        th = Some(match th {
            None => hx,
            Some(t) => node_hash(&hx, &t),
        });
    }
    if th != Some(tree.hash) {
        return Err("the log's tiles don't match the signed tree".into());
    }
    // Full tiles below them must match their parents.
    let first = stx_order.last().copied().unwrap_or(0) + 1;
    for i in first..tiles.len() {
        let t = tiles[i];
        let p = tile_parent(t, 1, tree.n).ok_or("bad math reading tiles")?;
        let j = *order.get(&p).ok_or("bad math reading tiles")?;
        let hx = hash_from_tile(&p, &data[j], stored_hash_index(p.l * p.h, t.n))?;
        if hx != tile_hash(&data[i]) {
            return Err("the log's tiles don't match the signed tree".into());
        }
    }
    let mut hashes = Vec::with_capacity(indexes.len());
    for (i, x) in indexes.iter().enumerate() {
        let j = index_order[i];
        hashes.push(hash_from_tile(&tiles[j], &data[j], *x)?);
    }
    Ok((hashes, tiles.into_iter().zip(data).collect()))
}

/// A lookup answer, authenticated: the tiles that proved it (to cache).
pub struct Verified {
    pub tiles: Vec<(Tile, Vec<u8>)>,
}

/// The go.sum lines a lookup answer records (empty if it is malformed).
pub fn record_lines(body: &[u8]) -> Vec<String> {
    match parse_record(body) {
        Ok((_, text, _)) => String::from_utf8_lossy(text)
            .lines()
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Authenticate a `/lookup/` answer the way the go command does.
pub async fn verify_lookup<F, Fut>(
    body: &[u8],
    v: &Verifier,
    read_tiles: F,
) -> Result<Verified, String>
where
    F: FnOnce(Vec<Tile>) -> Fut,
    Fut: Future<Output = Result<Vec<Vec<u8>>, String>>,
{
    let (id, text, note) = parse_record(body)?;
    let tree = parse_tree(open_note(note, v)?)?;
    if id >= tree.n {
        return Err(format!("record {id} is not in the tree of size {}", tree.n));
    }
    let (hashes, tiles) = read_hashes(tree, &[stored_hash_index(0, id)], read_tiles).await?;
    if hashes[0] != record_hash(text) {
        return Err("the record doesn't match the log".into());
    }
    Ok(Verified { tiles })
}

/// The `h1:` hashes a go.sum record gives `module@version`: the zip's and
/// the go.mod's.
pub fn sums(lines: &[String], module: &str, version: &str) -> (Option<String>, Option<String>) {
    let mut zip = None;
    let mut go_mod = None;
    for line in lines {
        let mut f = line.split(' ');
        let (Some(m), Some(v), Some(h), None) = (f.next(), f.next(), f.next(), f.next()) else {
            continue;
        };
        if m != module {
            continue;
        }
        if v == version {
            zip = Some(h.to_string());
        } else if v.strip_suffix("/go.mod") == Some(version) {
            go_mod = Some(h.to_string());
        }
    }
    (zip, go_mod)
}

/// A test log: records, tiles and signed notes, made the way
/// sum.golang.org makes them.
#[cfg(test)]
pub mod testlog {
    use super::*;

    pub struct Log {
        pub name: String,
        key: ring::signature::Ed25519KeyPair,
        pub vkey: String,
        /// Every stored hash, by stored hash index.
        stored: Vec<Hash>,
        pub records: Vec<Vec<u8>>,
    }

    impl Log {
        pub fn new(name: &str) -> Self {
            use ring::signature::KeyPair;
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
            let mut pubkey = vec![1u8];
            pubkey.extend_from_slice(key.public_key().as_ref());
            let vkey = format!(
                "{name}+{:08x}+{}",
                key_hash(name, &pubkey),
                STANDARD.encode(&pubkey)
            );
            Self {
                name: name.into(),
                key,
                vkey,
                stored: Vec::new(),
                records: Vec::new(),
            }
        }

        /// Append a record (go.sum lines); returns its id.
        pub fn add(&mut self, text: &str) -> i64 {
            let n = self.records.len() as i64;
            self.records.push(text.as_bytes().to_vec());
            // tlog.StoredHashes: the leaf, then each completed parent.
            let mut h = record_hash(text.as_bytes());
            self.stored.push(h);
            let mut level = 0u32;
            let mut m = n;
            while m & 1 == 1 {
                let left = self.stored[stored_hash_index(level, m - 1) as usize];
                h = node_hash(&left, &h);
                level += 1;
                m >>= 1;
                self.stored.push(h);
            }
            n
        }

        pub fn tree(&self) -> Tree {
            let n = self.records.len() as i64;
            let idx = sub_tree_index(0, n);
            let hs: Vec<Hash> = idx.iter().map(|i| self.stored[*i as usize]).collect();
            let mut th = *hs.last().unwrap();
            for h in hs.iter().rev().skip(1) {
                th = node_hash(h, &th);
            }
            Tree { n, hash: th }
        }

        pub fn signed_tree(&self) -> String {
            let t = self.tree();
            let text = format!(
                "go.sum database tree\n{}\n{}\n",
                t.n,
                STANDARD.encode(t.hash)
            );
            let sig = self.key.sign(text.as_bytes());
            let mut b = key_hash_of(&self.vkey).to_be_bytes().to_vec();
            b.extend_from_slice(sig.as_ref());
            format!("{text}\n{SIG_PREFIX}{} {}\n", self.name, STANDARD.encode(b))
        }

        pub fn lookup(&self, id: i64) -> Vec<u8> {
            let mut out = format!("{id}\n").into_bytes();
            out.extend_from_slice(&self.records[id as usize]);
            out.push(b'\n');
            out.extend_from_slice(self.signed_tree().as_bytes());
            out
        }

        /// A tile's data, as served at its path.
        pub fn tile(&self, t: &Tile) -> Vec<u8> {
            let mut out = Vec::new();
            for i in 0..t.w {
                let level = t.l * t.h;
                let n = (t.n << t.h) + i;
                out.extend_from_slice(&self.stored[stored_hash_index(level, n) as usize]);
            }
            out
        }
    }

    fn key_hash_of(vkey: &str) -> u32 {
        u32::from_str_radix(vkey.split('+').nth(1).unwrap(), 16).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_key_parses() {
        let v = Verifier::parse(SUM_GOLANG_ORG_KEY).unwrap();
        assert_eq!(v.name, "sum.golang.org");
        assert!(
            Verifier::parse("sum.golang.org+033de0ae+Ac4zctda0e5eza+HJyk9SxEdh+s3Ki0ZmGW+ZeJW9eA=")
                .is_err()
        );
        assert!(Verifier::parse("nonsense").is_err());
    }

    #[test]
    fn tile_paths() {
        let t = Tile {
            h: 8,
            l: 0,
            n: 1234067,
            w: 256,
        };
        assert_eq!(t.path(), "tile/8/0/x001/x234/067");
        let t = Tile {
            h: 8,
            l: 1,
            n: 3,
            w: 5,
        };
        assert_eq!(t.path(), "tile/8/1/003.p/5");
    }

    #[test]
    fn index_math_round_trips() {
        for level in 0..5 {
            for n in 0..300 {
                let i = stored_hash_index(level, n);
                assert_eq!(split_stored_hash_index(i), (level, n));
            }
        }
    }

    async fn check_log(records: usize) {
        let mut log = testlog::Log::new("test.example");
        for i in 0..records {
            log.add(&format!(
                "example.com/m{i} v1.0.0 h1:AAAA\nexample.com/m{i} v1.0.0/go.mod h1:BBBB\n"
            ));
        }
        let v = Verifier::parse(&log.vkey).unwrap();
        for id in [0, records as i64 / 2, records as i64 - 1] {
            let body = log.lookup(id);
            let read = |ts: Vec<Tile>| {
                let data: Vec<Vec<u8>> = ts.iter().map(|t| log.tile(t)).collect();
                async { Ok(data) }
            };
            verify_lookup(&body, &v, read).await.unwrap();
            let lines = record_lines(&body);
            assert_eq!(lines[0], format!("example.com/m{id} v1.0.0 h1:AAAA"));
            assert_eq!(
                sums(&lines, &format!("example.com/m{id}"), "v1.0.0"),
                (Some("h1:AAAA".into()), Some("h1:BBBB".into()))
            );
        }
    }

    #[tokio::test]
    async fn verifies_records_in_logs_of_any_size() {
        for n in [1, 2, 3, 7, 255, 256, 257, 1000, 70_000] {
            check_log(n).await;
        }
    }

    /// Against the real sum.golang.org, with its real key. Skipped without
    /// network access.
    #[tokio::test]
    async fn verifies_a_real_sum_golang_org_lookup() {
        let client = reqwest::Client::new();
        let get = |path: String| {
            let client = client.clone();
            async move {
                let r = client
                    .get(format!("https://sum.golang.org/{path}"))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if !r.status().is_success() {
                    return Err(format!("{path}: {}", r.status()));
                }
                r.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| e.to_string())
            }
        };
        let Ok(body) = get("lookup/rsc.io/quote@v1.5.2".into()).await else {
            return eprintln!("skipping: sum.golang.org is not reachable");
        };
        let v = Verifier::parse(SUM_GOLANG_ORG_KEY).unwrap();
        let read = |ts: Vec<Tile>| async move {
            let mut out = Vec::new();
            for t in ts {
                out.push(get(t.path()).await?);
            }
            Ok(out)
        };
        verify_lookup(&body, &v, read).await.unwrap();
        assert_eq!(
            sums(&record_lines(&body), "rsc.io/quote", "v1.5.2"),
            (
                Some("h1:w5fcysjrx7yqtD/aO+QwRjYZOKnaM9Uh2b40tElTs3Y=".into()),
                Some("h1:LzX7hefJvL54yjefDEDHNONDjII0t9xZLPXsUe+TKr0=".into())
            )
        );
    }

    #[tokio::test]
    async fn refuses_forged_answers() {
        let mut log = testlog::Log::new("test.example");
        for i in 0..20 {
            log.add(&format!("example.com/m{i} v1.0.0 h1:AAAA\n"));
        }
        let v = Verifier::parse(&log.vkey).unwrap();
        let read = |ts: Vec<Tile>| {
            let data: Vec<Vec<u8>> = ts.iter().map(|t| log.tile(t)).collect();
            async { Ok(data) }
        };
        // A changed record, with the genuine signed tree.
        let body = String::from_utf8(log.lookup(5))
            .unwrap()
            .replace("h1:AAAA", "h1:EVIL");
        let err = verify_lookup(body.as_bytes(), &v, read)
            .await
            .err()
            .unwrap();
        assert!(err.contains("doesn't match"), "{err}");
        // Someone else's key.
        let other = testlog::Log::new("test.example");
        let ov = Verifier::parse(&other.vkey).unwrap();
        assert!(verify_lookup(&log.lookup(5), &ov, read).await.is_err());
        // A tampered tile.
        let bad_read = |ts: Vec<Tile>| {
            let mut data: Vec<Vec<u8>> = ts.iter().map(|t| log.tile(t)).collect();
            data[0][0] ^= 1;
            async { Ok(data) }
        };
        assert!(verify_lookup(&log.lookup(5), &v, bad_read).await.is_err());
        // A tampered signature.
        let body = String::from_utf8(log.lookup(5)).unwrap();
        let mut forged = body.into_bytes();
        let n = forged.len();
        forged[n - 3] = if forged[n - 3] == b'A' { b'B' } else { b'A' };
        assert!(verify_lookup(&forged, &v, read).await.is_err());
    }
}
