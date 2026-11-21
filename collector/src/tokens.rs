//! "Tokens today" from local transcripts of Claude Code (`~/.claude/projects/**/*.jsonl`) and
//! Gemini / Antigravity CLI (`~/.gemini/antigravity-cli/brain/**/transcript.jsonl`), computed
//! incrementally.
//!
//! Semantics match a full depth-first scan (depth 4, regular files with `mtime >= midnight`,
//! UTF-8 lines stopping at any non-UTF-8 line, first occurrence in walk + line order wins):
//! - Claude: `*.jsonl`, `type == "assistant"`, `timestamp >= midnight`, `message.usage` present,
//!   dedupe key `message.id + "|" + requestId` (`"|"` is never deduped).
//! - Gemini: exact file name `"transcript.jsonl"` (never `"transcript_full.jsonl"`),
//!   `type == "PLANNER_RESPONSE"`, `created_at >= midnight`, at least one token field present,
//!   dedupe key `(file_path, step_index)` (`step_index: None` is never deduped).
//!
//! What makes it fast: a small binary cache in `~/.cache/agent-usage/` remembers, per file, how far it
//! was read (always just after a newline), a fingerprint of the bytes before that point, and the
//! newest entry timestamp seen; plus, for today, the winning occurrence of every dedupe key. Each run
//! only reads bytes appended since the previous run. Because the winner of a key is chosen by
//! position (walk order, offset) rather than by when it was read, the result does not depend on when
//! the runs happened.
//!
//! Midnight: the cache belongs to one local day. On a new day the per-key winners are dropped and a
//! file's already-read prefix is skipped only if its newest entry is older than the new midnight (so
//! it cannot contain an entry that counts today); otherwise that file is read again from the start.
//! Anything unexpected (a file replaced, truncated or rewritten, or a file that counted today being
//! deleted) rebuilds today's totals the same way. Cold scans read big files in parallel chunks.

use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, UNIX_EPOCH};

use memchr::memmem;

/// Read big regions in chunks of this size (aligned to line ends), in parallel.
const CHUNK: u64 = 1 << 20;
const MAX_THREADS: usize = 8;
/// Bytes hashed at the start of a file and just before the read offset to notice rewrites.
const FP: u64 = 64;
const NONE_TS: i64 = i64::MIN;

struct Spec {
    label: &'static str,
    rel_root: &'static str,
    cache_file: &'static str,
    magic: &'static [u8; 16],
    match_file: fn(&Path) -> bool,
    finder_a: &'static [u8],
    finder_b: Option<&'static [u8]>,
    scan_line: fn(&[u8], &[u8], u64, i64, &Finders, &mut Vec<Hit>, &mut i64),
}

const CLAUDE_SPEC: Spec = Spec {
    label: "claude tokens",
    rel_root: ".claude/projects",
    cache_file: "claude-tokens-v1.bin",
    magic: b"AU-CLAUDE-TOK-1\n",
    match_file: |p| p.extension().is_some_and(|x| x == "jsonl"),
    finder_a: b"\"usage\"",
    finder_b: Some(b"\"assistant\""),
    scan_line: scan_claude_line,
};

const GEMINI_SPEC: Spec = Spec {
    label: "gemini tokens",
    rel_root: ".gemini/antigravity-cli/brain",
    cache_file: "gemini-tokens-v1.bin",
    magic: b"AU-GEMINI-TOK-1\n",
    match_file: |p| p.file_name().is_some_and(|x| x == "transcript.jsonl"),
    finder_a: b"\"PLANNER_RESPONSE\"",
    finder_b: None,
    scan_line: scan_gemini_line,
};

// ---------- transcript entries (only the fields we need; serde skips the rest) ----------

#[derive(Deserialize)]
struct ClaudeEntry<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(rename = "requestId", borrow)]
    request_id: Option<Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<ClaudeMsg<'a>>,
}
#[derive(Deserialize)]
struct ClaudeMsg<'a> {
    #[serde(borrow)]
    id: Option<Cow<'a, str>>,
    usage: Option<ClaudeUsage>,
}
#[derive(Deserialize)]
struct ClaudeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct GeminiEntry<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    created_at: Option<Cow<'a, str>>,
    step_index: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
}

/// [uncached input, output, cache write, cache read]
type Tok = [u64; 4];

fn add(a: &mut Tok, b: &Tok) {
    for i in 0..4 {
        a[i] = a[i].wrapping_add(b[i]);
    }
}
fn sub(a: &mut Tok, b: &Tok) {
    for i in 0..4 {
        a[i] = a[i].wrapping_sub(b[i]);
    }
}

/// One counted entry: byte offset of its line, dedupe key hash (None = never deduped), usage.
struct Hit {
    off: u64,
    key: Option<u64>,
    tok: Tok,
}

// ---------- hashing (stable across builds: the cache stores these) ----------

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
const FNV0: u64 = 0xcbf2_9ce4_8422_2325;
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}
/// Hash of the original Claude key string `format!("{id}|{rid}")`.
fn claude_key_hash(id: &str, rid: &str) -> u64 {
    mix(fnv(fnv(fnv(FNV0, id.as_bytes()), b"|"), rid.as_bytes()))
}
/// Hash of `(file_path_bytes, step_index)`: NUL cannot appear in a Unix path, so keys from
/// different conversation files never collide while duplicate `step_index` lines in one file do.
fn gemini_key_hash(file_path: &[u8], idx: u64) -> u64 {
    mix(fnv(fnv(fnv(FNV0, file_path), &[0]), &idx.to_le_bytes()))
}

// ---------- the cache ----------

#[derive(Clone)]
struct FileState {
    path: Vec<u8>,
    dev: u64,
    ino: u64,
    /// Bytes [0, offset) have been read; offset is 0 or just after a '\n' (or at a stop line).
    offset: u64,
    mtime_ns: i64,
    /// Newest timestamp of any countable entry in [0, offset).
    max_ts: i64,
    head: u64,
    tail: u64,
    /// A non-UTF-8 line starts at `offset`: like the original, never read past it.
    stopped: bool,
}

impl FileState {
    fn reset(&mut self) {
        self.offset = 0;
        self.max_ts = NONE_TS;
        self.head = 0;
        self.tail = 0;
        self.stopped = false;
        self.mtime_ns = i64::MIN;
    }
}

#[derive(Clone, Copy)]
struct Winner {
    file: u32,
    off: u64,
    tok: Tok,
}

struct Cache {
    root: Vec<u8>,
    midnight: i64,
    files: Vec<FileState>,
    nokey: Tok,
    winners: HashMap<u64, Winner>,
}

impl Cache {
    fn empty(root: &[u8], midnight: i64) -> Cache {
        Cache { root: root.to_vec(), midnight, files: Vec::new(), nokey: [0; 4], winners: HashMap::new() }
    }

    fn encode(&self, magic: &[u8; 16]) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.files.len() * 120 + self.winners.len() * 52);
        b.extend_from_slice(magic);
        put_bytes(&mut b, &self.root);
        put(&mut b, self.midnight as u64);
        put32(&mut b, self.files.len() as u32);
        for f in &self.files {
            put_bytes(&mut b, &f.path);
            for v in [f.dev, f.ino, f.offset, f.mtime_ns as u64, f.max_ts as u64, f.head, f.tail] {
                put(&mut b, v);
            }
            b.push(f.stopped as u8);
        }
        for v in self.nokey {
            put(&mut b, v);
        }
        put32(&mut b, self.winners.len() as u32);
        for (k, w) in &self.winners {
            put(&mut b, *k);
            put32(&mut b, w.file);
            put(&mut b, w.off);
            for v in w.tok {
                put(&mut b, v);
            }
        }
        let sum = fnv(FNV0, &b);
        put(&mut b, sum);
        b
    }

    fn decode(b: &[u8], magic: &[u8; 16]) -> Option<Cache> {
        if b.len() < magic.len() + 8 || &b[..magic.len()] != magic {
            return None;
        }
        let (body, sum) = b.split_at(b.len() - 8);
        if fnv(FNV0, body) != u64::from_le_bytes(sum.try_into().ok()?) {
            return None;
        }
        let mut r = Rd { b: body, p: magic.len() };
        let root = r.bytes()?.to_vec();
        let midnight = r.u64()? as i64;
        let nf = r.u32()? as usize;
        let mut files = Vec::with_capacity(nf.min(1 << 16));
        for _ in 0..nf {
            let path = r.bytes()?.to_vec();
            let (dev, ino, offset) = (r.u64()?, r.u64()?, r.u64()?);
            let (mtime_ns, max_ts) = (r.u64()? as i64, r.u64()? as i64);
            let (head, tail) = (r.u64()?, r.u64()?);
            let stopped = r.u8()? != 0;
            files.push(FileState { path, dev, ino, offset, mtime_ns, max_ts, head, tail, stopped });
        }
        let nokey = [r.u64()?, r.u64()?, r.u64()?, r.u64()?];
        let nw = r.u32()? as usize;
        let mut winners = HashMap::with_capacity(nw.min(1 << 20));
        for _ in 0..nw {
            let k = r.u64()?;
            let file = r.u32()?;
            let off = r.u64()?;
            let tok = [r.u64()?, r.u64()?, r.u64()?, r.u64()?];
            if file as usize >= files.len() {
                return None;
            }
            winners.insert(k, Winner { file, off, tok });
        }
        if r.p != body.len() {
            return None;
        }
        Some(Cache { root, midnight, files, nokey, winners })
    }
}

fn put(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(b: &mut Vec<u8>, v: &[u8]) {
    put32(b, v.len() as u32);
    b.extend_from_slice(v);
}
struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

pub(crate) fn cache_dir(h: &Path) -> Option<PathBuf> {
    if std::env::var_os("AGENT_USAGE_NO_CACHE").is_some_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    match std::env::var_os("AGENT_USAGE_CACHE_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(h.join(".cache/agent-usage")),
    }
}

fn load(dir: &Path, spec: &Spec) -> Option<Cache> {
    let mut b = Vec::new();
    File::open(dir.join(spec.cache_file)).ok()?.read_to_end(&mut b).ok()?;
    Cache::decode(&b, spec.magic)
}

fn save(dir: &Path, spec: &Spec, c: &Cache) {
    if !crate::private_dir(dir) {
        return;
    }
    let cache_file = spec.cache_file;
    let tmp = dir.join(format!(".{cache_file}.{}", std::process::id()));
    let ok = crate::create_temp(&tmp).and_then(|mut f| f.write_all(&c.encode(spec.magic))).is_ok();
    if !ok || fs::rename(&tmp, dir.join(cache_file)).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

// ---------- walking ----------

struct Found {
    path: PathBuf,
    md: fs::Metadata,
    /// mtime >= midnight, as the original filters.
    included: bool,
}

/// Depth-first walk using `DirEntry::metadata` (does not follow symlinks).
/// Only regular files matching `match_file` count: symlinks, FIFOs and devices are skipped.
fn walk(dir: &Path, since: i64, depth: u32, match_file: fn(&Path) -> bool, out: &mut Vec<Found>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            if depth > 0 {
                walk(&p, since, depth - 1, match_file, out);
            }
        } else if md.is_file() && match_file(&p) {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            out.push(Found { path: p, md, included: mtime >= since });
        }
    }
}

fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.mtime().wrapping_mul(1_000_000_000).wrapping_add(md.mtime_nsec())
}

/// (head hash, tail hash) of a file read up to `offset`.
fn fingerprint(f: &File, offset: u64) -> Option<(u64, u64)> {
    let mut buf = [0u8; FP as usize];
    let n = FP.min(offset) as usize;
    f.read_exact_at(&mut buf[..n], 0).ok()?;
    let head = fnv(FNV0, &buf[..n]);
    f.read_exact_at(&mut buf[..n], offset - n as u64).ok()?;
    let tail = fnv(FNV0, &buf[..n]);
    Some((head, tail))
}

// ---------- scanning ----------

struct Job {
    plan: usize,
    start: u64,
    /// None: read to end of file (the last chunk of a file; may end with an unterminated line).
    end: Option<u64>,
}

#[derive(Default)]
struct JobOut {
    hits: Vec<Hit>,
    max_ts: i64,
    /// Offset just after the last '\n' read.
    done: u64,
    /// A complete line that is not UTF-8 starts here: stop reading the file there.
    stop: Option<u64>,
    /// Entries in an unterminated last line (counted this run, not persisted).
    tail: Vec<Hit>,
    io_error: bool,
    bytes: u64,
}

struct Finders {
    a: memmem::Finder<'static>,
    b: Option<memmem::Finder<'static>>,
}

fn scan_claude_line(
    line: &[u8],
    _file_path: &[u8],
    off: u64,
    midnight: i64,
    fd: &Finders,
    hits: &mut Vec<Hit>,
    max_ts: &mut i64,
) {
    if fd.a.find(line).is_none() || fd.b.as_ref().is_some_and(|b| b.find(line).is_none()) {
        return;
    }
    let Ok(e) = serde_json::from_slice::<ClaudeEntry>(line) else { return };
    if e.kind.as_deref() != Some("assistant") {
        return;
    }
    let Some(t) = e.timestamp.as_deref().and_then(crate::parse_iso) else { return };
    let Some(m) = e.message else { return };
    let Some(u) = m.usage else { return };
    if t > *max_ts {
        *max_ts = t;
    }
    if t < midnight {
        return;
    }
    let id = m.id.as_deref().unwrap_or("");
    let rid = e.request_id.as_deref().unwrap_or("");
    let key = if id.is_empty() && rid.is_empty() { None } else { Some(claude_key_hash(id, rid)) };
    let tok = [
        u.input_tokens.unwrap_or(0),
        u.output_tokens.unwrap_or(0),
        u.cache_creation_input_tokens.unwrap_or(0),
        u.cache_read_input_tokens.unwrap_or(0),
    ];
    hits.push(Hit { off, key, tok });
}

fn scan_gemini_line(
    line: &[u8],
    file_path: &[u8],
    off: u64,
    midnight: i64,
    fd: &Finders,
    hits: &mut Vec<Hit>,
    max_ts: &mut i64,
) {
    if fd.a.find(line).is_none() {
        return;
    }
    let Ok(e) = serde_json::from_slice::<GeminiEntry>(line) else { return };
    if e.kind.as_deref() != Some("PLANNER_RESPONSE") {
        return;
    }
    let Some(t) = e.created_at.as_deref().and_then(crate::parse_iso) else { return };
    if e.input_tokens.is_none()
        && e.output_tokens.is_none()
        && e.cache_creation_tokens.is_none()
        && e.cache_read_tokens.is_none()
    {
        return;
    }
    if t > *max_ts {
        *max_ts = t;
    }
    if t < midnight {
        return;
    }
    let key = e.step_index.map(|idx| gemini_key_hash(file_path, idx));
    let tok = [
        e.input_tokens.unwrap_or(0),
        e.output_tokens.unwrap_or(0),
        e.cache_creation_tokens.unwrap_or(0),
        e.cache_read_tokens.unwrap_or(0),
    ];
    hits.push(Hit { off, key, tok });
}

/// Open a transcript: never through a symlink, never blocking (e.g. on a FIFO), regular files only.
fn open_regular(p: &Path) -> std::io::Result<File> {
    let f = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(p)?;
    if f.metadata()?.is_file() {
        Ok(f)
    } else {
        Err(std::io::Error::new(ErrorKind::InvalidInput, "not a regular file"))
    }
}

/// Largest region read in one piece. A chunk ends at the first newline after 1 MiB, so only an
/// absurdly long line (or an unterminated multi-GB tail) gets near it; such a region counts as
/// unreadable instead of exhausting memory.
const MAX_REGION: u64 = 256 << 20;

fn too_big() -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, "region too large")
}

fn read_region(f: &File, start: u64, end: Option<u64>, hint: u64) -> std::io::Result<Vec<u8>> {
    match end {
        Some(e) => {
            if e.saturating_sub(start) > MAX_REGION {
                return Err(too_big());
            }
            let mut buf = vec![0u8; (e - start) as usize];
            f.read_exact_at(&mut buf, start)?;
            Ok(buf)
        }
        None => {
            let mut buf = Vec::with_capacity(hint.min(MAX_REGION) as usize + 4096);
            let mut pos = start;
            loop {
                if buf.len() as u64 > MAX_REGION {
                    return Err(too_big());
                }
                if buf.capacity() - buf.len() < 64 * 1024 {
                    buf.reserve(1 << 20);
                }
                let len = buf.len();
                let cap = buf.capacity();
                buf.resize(cap, 0);
                match f.read_at(&mut buf[len..], pos) {
                    Ok(0) => {
                        buf.truncate(len);
                        return Ok(buf);
                    }
                    Ok(n) => {
                        buf.truncate(len + n);
                        pos += n as u64;
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => buf.truncate(len),
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

fn scan_job(
    f: &File,
    file_path: &[u8],
    job: &Job,
    hint: u64,
    midnight: i64,
    fd: &Finders,
    spec: &Spec,
) -> JobOut {
    let mut out = JobOut { max_ts: NONE_TS, done: job.start, ..Default::default() };
    let buf = match read_region(f, job.start, job.end, hint) {
        Ok(b) => b,
        Err(_) => {
            out.io_error = true;
            return out;
        }
    };
    out.bytes = buf.len() as u64;
    let mut pos = 0usize;
    while pos < buf.len() {
        let off = job.start + pos as u64;
        match memchr::memchr(b'\n', &buf[pos..]) {
            Some(i) => {
                let line = &buf[pos..pos + i];
                if std::str::from_utf8(line).is_err() {
                    out.stop = Some(off);
                    break;
                }
                (spec.scan_line)(line, file_path, off, midnight, fd, &mut out.hits, &mut out.max_ts);
                pos += i + 1;
                out.done = job.start + pos as u64;
            }
            None => {
                // Unterminated last line: read as a line too (not persisted until terminated).
                let line = &buf[pos..];
                if std::str::from_utf8(line).is_ok() {
                    let mut ignored = NONE_TS;
                    (spec.scan_line)(line, file_path, off, midnight, fd, &mut out.tail, &mut ignored);
                }
                break;
            }
        }
    }
    out
}

/// Split [start, size) into chunks that end just after a '\n'; the last one reads to EOF.
fn split(f: &File, plan: usize, start: u64, size: u64, jobs: &mut Vec<Job>) {
    let mut s = start;
    let mut probe = vec![0u8; 64 * 1024];
    while size > s && size - s > CHUNK + CHUNK / 2 {
        // Find the first '\n' at or after s + CHUNK - 1.
        let mut p = s + CHUNK - 1;
        let mut cut = None;
        while p < size {
            let n = probe.len().min((size - p) as usize);
            if f.read_exact_at(&mut probe[..n], p).is_err() {
                break;
            }
            if let Some(i) = memchr::memchr(b'\n', &probe[..n]) {
                cut = Some(p + i as u64 + 1);
                break;
            }
            p += n as u64;
        }
        match cut {
            Some(c) if c < size => {
                jobs.push(Job { plan, start: s, end: Some(c) });
                s = c;
            }
            _ => break,
        }
    }
    jobs.push(Job { plan, start: s, end: None });
}

pub struct Report {
    pub value: serde_json::Value,
    pub trace: String,
}

/// Tokens Claude Code used on this machine today, from local session transcripts.
pub fn claude_tokens_today(h: &Path, midnight: i64) -> Report {
    tokens_today(h, midnight, &CLAUDE_SPEC)
}

/// Tokens Gemini (Antigravity CLI) used on this machine today, from local conversation transcripts.
pub fn gemini_tokens_today(h: &Path, midnight: i64) -> Report {
    tokens_today(h, midnight, &GEMINI_SPEC)
}

fn tokens_today(h: &Path, midnight: i64, spec: &Spec) -> Report {
    let t0 = Instant::now();
    let root = h.join(spec.rel_root);
    let root_b = root.as_os_str().as_encoded_bytes().to_vec();
    let mut found = Vec::new();
    walk(&root, midnight, 4, spec.match_file, &mut found);
    let t_walk = t0.elapsed();

    let dir = cache_dir(h);
    let loaded = dir.as_deref().and_then(|d| load(d, spec)).filter(|c| c.root == root_b);
    let warm = loaded.is_some();
    let mut c = loaded.unwrap_or_else(|| Cache::empty(&root_b, midnight));
    let mut dirty = !warm;
    let mut new_day = false;
    if c.midnight != midnight {
        // A new local day (or a timezone change): nothing counted so far is today's.
        new_day = true;
        dirty = true;
        c.midnight = midnight;
        c.winners.clear();
        c.nokey = [0; 4];
        for f in &mut c.files {
            if f.max_ts >= midnight {
                f.reset();
            }
        }
    }

    let index: HashMap<Vec<u8>, usize> =
        c.files.iter().enumerate().map(|(i, f)| (f.path.clone(), i)).collect();
    // Walk position of each cache entry (None: no longer on disk); new files are appended to c.files.
    let mut order: Vec<Option<u32>> = vec![None; c.files.len()];
    let mut rebuild = false;
    // (cache index, metadata) of included files, in walk order.
    let mut todo: Vec<(usize, &fs::Metadata)> = Vec::new();
    for (pos, fnd) in found.iter().enumerate() {
        let pb = fnd.path.as_os_str().as_encoded_bytes();
        let i = match index.get(pb) {
            Some(&i) => i,
            None => {
                if !fnd.included {
                    continue; // never read and not modified today: nothing to remember
                }
                c.files.push(FileState {
                    path: pb.to_vec(),
                    dev: 0,
                    ino: 0,
                    offset: 0,
                    mtime_ns: i64::MIN,
                    max_ts: NONE_TS,
                    head: 0,
                    tail: 0,
                    stopped: false,
                });
                order.push(None);
                dirty = true;
                c.files.len() - 1
            }
        };
        order[i] = Some(pos as u32);
        if fnd.included {
            todo.push((i, &fnd.md));
        } else if c.files[i].max_ts >= midnight {
            rebuild = true; // counted today, but no longer included (mtime moved back)
        }
    }
    for (i, f) in c.files.iter().enumerate() {
        if order[i].is_none() && f.max_ts >= midnight {
            rebuild = true; // a file that counted today is gone
        }
    }

    // Check each included file against its cache entry; open the ones with something to read.
    struct Plan {
        idx: usize,
        path: Vec<u8>,
        file: File,
        start: u64,
        size: u64,
        dev: u64,
        ino: u64,
        mtime_ns: i64,
    }
    let mut plans: Vec<Plan> = Vec::new();
    let mut checked: Vec<(usize, File, u64, u64, u64, i64)> = Vec::new(); // idx, file, size, dev, ino, mtime
    for &(i, md) in &todo {
        let fs_ = &c.files[i];
        let unchanged = !md.file_type().is_symlink()
            && md.dev() == fs_.dev
            && md.ino() == fs_.ino
            && md.len() == fs_.offset
            && mtime_ns(md) == fs_.mtime_ns
            && fs_.mtime_ns != i64::MIN;
        if unchanged {
            continue;
        }
        let Ok(file) = open_regular(bytes_path(&fs_.path)) else {
            // Unreadable now: skips it, so it must not keep counting from the cache.
            if fs_.max_ts >= midnight {
                rebuild = true;
            }
            c.files[i].reset();
            dirty = true;
            continue;
        };
        let Ok(fmd) = file.metadata() else { continue };
        let (dev, ino, size) = (fmd.dev(), fmd.ino(), fmd.len());
        let mut consistent = fs_.offset == 0 || (dev == fs_.dev && ino == fs_.ino && size >= fs_.offset);
        if consistent && fs_.offset > 0 {
            consistent = fingerprint(&file, fs_.offset) == Some((fs_.head, fs_.tail));
        }
        if !consistent {
            if fs_.max_ts >= midnight {
                rebuild = true;
            }
            c.files[i].reset();
        }
        checked.push((i, file, size, dev, ino, mtime_ns(md)));
    }

    if rebuild {
        // Recount today from scratch; a prefix whose newest entry predates midnight is still skipped.
        dirty = true;
        c.winners.clear();
        c.nokey = [0; 4];
        let have: std::collections::HashSet<usize> = checked.iter().map(|x| x.0).collect();
        for f in &mut c.files {
            if f.max_ts >= midnight {
                f.reset();
            }
        }
        // Files skipped above as unchanged may have been reset: open them too.
        for &(i, md) in &todo {
            if have.contains(&i) || c.files[i].offset != 0 {
                continue;
            }
            let p = bytes_path(&c.files[i].path).to_path_buf();
            if let Ok(file) = open_regular(&p) {
                if let Ok(fmd) = file.metadata() {
                    checked.push((i, file, fmd.len(), fmd.dev(), fmd.ino(), mtime_ns(md)));
                }
            }
        }
    }

    for (idx, file, size, dev, ino, mt) in checked {
        let fs_ = &mut c.files[idx];
        if fs_.stopped || size <= fs_.offset {
            // Nothing new to read (or never past a stop line): just remember the new mtime.
            if fs_.mtime_ns != mt || fs_.dev != dev || fs_.ino != ino {
                fs_.mtime_ns = mt;
                fs_.dev = dev;
                fs_.ino = ino;
                dirty = true;
            }
            continue;
        }
        plans.push(Plan {
            idx,
            path: fs_.path.clone(),
            file,
            start: fs_.offset,
            size,
            dev,
            ino,
            mtime_ns: mt,
        });
    }

    // Read everything new, in parallel chunks.
    let mut jobs = Vec::new();
    for (pi, p) in plans.iter().enumerate() {
        split(&p.file, pi, p.start, p.size, &mut jobs);
    }
    let fd = Finders {
        a: memmem::Finder::new(spec.finder_a),
        b: spec.finder_b.map(memmem::Finder::new),
    };
    let mut outs: Vec<Option<JobOut>> = (0..jobs.len()).map(|_| None).collect();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(MAX_THREADS).min(jobs.len());
    let hint = |j: &Job| match j.end {
        Some(e) => e - j.start,
        None => plans[j.plan].size.saturating_sub(j.start),
    };
    if threads <= 1 {
        for (k, j) in jobs.iter().enumerate() {
            let p = &plans[j.plan];
            outs[k] = Some(scan_job(&p.file, &p.path, j, hint(j), midnight, &fd, spec));
        }
    } else {
        let next = AtomicUsize::new(0);
        let work = || {
            let mut mine = Vec::new();
            loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                let Some(j) = jobs.get(k) else { break };
                let p = &plans[j.plan];
                mine.push((k, scan_job(&p.file, &p.path, j, hint(j), midnight, &fd, spec)));
            }
            mine
        };
        let results: Vec<Vec<(usize, JobOut)>> = std::thread::scope(|s| {
            // A thread that cannot be created is simply missing (the others, or this one, do its work).
            let hs: Vec<_> =
                (0..threads).filter_map(|_| std::thread::Builder::new().spawn_scoped(s, work).ok()).collect();
            let mut all: Vec<_> = hs.into_iter().map(|h| h.join().unwrap_or_default()).collect();
            all.push(work());
            all
        });
        for (k, o) in results.into_iter().flatten() {
            outs[k] = Some(o);
        }
    }
    let t_scan = t0.elapsed();

    // Merge, file by file, chunks in order.
    let mut bytes = 0u64;
    let mut provisional: Vec<(usize, Hit)> = Vec::new();
    let mut per_plan: Vec<Vec<JobOut>> = (0..plans.len()).map(|_| Vec::new()).collect();
    for (k, j) in jobs.iter().enumerate() {
        per_plan[j.plan].push(outs[k].take().unwrap_or_else(|| JobOut { io_error: true, ..Default::default() }));
    }
    let mut new_hits: Vec<(usize, Hit)> = Vec::new();
    for (pi, p) in plans.iter().enumerate() {
        let parts = std::mem::take(&mut per_plan[pi]);
        if parts.iter().any(|o| o.io_error) {
            continue; // try again next run
        }
        let fs_ = &mut c.files[p.idx];
        let mut offset = fs_.offset;
        let mut max_ts = fs_.max_ts;
        let mut stopped = false;
        for o in parts {
            bytes += o.bytes;
            max_ts = max_ts.max(o.max_ts);
            new_hits.extend(o.hits.into_iter().map(|h| (p.idx, h)));
            if let Some(s) = o.stop {
                offset = s;
                stopped = true;
                break;
            }
            offset = o.done;
            provisional.extend(o.tail.into_iter().map(|h| (p.idx, h)));
        }
        let Some((head, tail)) = (if offset > 0 { fingerprint(&p.file, offset) } else { Some((0, 0)) }) else {
            continue;
        };
        *fs_ = FileState {
            path: std::mem::take(&mut fs_.path),
            dev: p.dev,
            ino: p.ino,
            offset,
            mtime_ns: p.mtime_ns,
            max_ts,
            head,
            tail,
            stopped,
        };
        dirty = true;
    }

    // The winner of a key is its first occurrence in walk order, then line order.
    let rank = |file: usize, off: u64| (order[file].unwrap_or(u32::MAX), off);
    for (fi, h) in new_hits {
        match h.key {
            None => add(&mut c.nokey, &h.tok),
            Some(k) => {
                let better = match c.winners.get(&k) {
                    None => true,
                    Some(w) => rank(fi, h.off) < rank(w.file as usize, w.off),
                };
                if better {
                    c.winners.insert(k, Winner { file: fi as u32, off: h.off, tok: h.tok });
                }
            }
        }
    }

    let mut total = c.nokey;
    for w in c.winners.values() {
        add(&mut total, &w.tok);
    }
    // Unterminated last lines count now (as in the original) but are read again next run.
    let mut overlay: HashMap<u64, (usize, u64, Tok)> = HashMap::new();
    for (fi, h) in provisional {
        match h.key {
            None => add(&mut total, &h.tok),
            Some(k) => {
                let cur = overlay
                    .get(&k)
                    .map(|&(f, o, t)| (rank(f, o), t))
                    .or_else(|| c.winners.get(&k).map(|w| (rank(w.file as usize, w.off), w.tok)));
                match cur {
                    None => {
                        add(&mut total, &h.tok);
                        overlay.insert(k, (fi, h.off, h.tok));
                    }
                    Some((r, t)) if rank(fi, h.off) < r => {
                        sub(&mut total, &t);
                        add(&mut total, &h.tok);
                        overlay.insert(k, (fi, h.off, h.tok));
                    }
                    _ => {}
                }
            }
        }
    }

    if let Some(d) = dir.as_deref() {
        // Forget files that are gone; remap winner file indices.
        if order.iter().any(|o| o.is_none()) {
            let mut remap = vec![u32::MAX; c.files.len()];
            let mut kept = Vec::with_capacity(c.files.len());
            for (i, f) in std::mem::take(&mut c.files).into_iter().enumerate() {
                if order[i].is_some() {
                    remap[i] = kept.len() as u32;
                    kept.push(f);
                }
            }
            c.files = kept;
            c.winners.retain(|_, w| remap[w.file as usize] != u32::MAX);
            for w in c.winners.values_mut() {
                w.file = remap[w.file as usize];
            }
            dirty = true;
        }
        if dirty {
            save(d, spec, &c);
        }
    }

    let [input, output, cache_w, cache_r] = total;
    let trace = format!(
        "{}: {} files walked ({} today), {} read ({} chunks, {} threads), {:.1} MB, cache {}{}{} | walk {:.1} ms, scan {:.1} ms, total {:.1} ms",
        spec.label,
        found.len(),
        todo.len(),
        plans.len(),
        jobs.len(),
        threads.max(1),
        bytes as f64 / 1e6,
        if warm { "warm" } else { "cold" },
        if new_day && warm { ", new day" } else { "" },
        if rebuild { ", rebuild" } else { "" },
        t_walk.as_secs_f64() * 1e3,
        (t_scan - t_walk).as_secs_f64() * 1e3,
        t0.elapsed().as_secs_f64() * 1e3,
    );
    Report {
        value: serde_json::json!({
            "today_total": input.wrapping_add(output).wrapping_add(cache_w).wrapping_add(cache_r),
            "today_output": output,
            "today_uncached_input": input,
            "source": "local transcripts on this computer",
        }),
        trace,
    }
}

/// Paths in the cache are the OS bytes of real paths (on Unix any bytes are a valid path).
fn bytes_path(b: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gemini_scanner_dedupes_within_file_not_across_files_and_ignores_full_transcript() {
        let dir = std::env::temp_dir().join(format!("au-gemini-tok-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let midnight = 1_763_596_800i64; // 2025-11-20T00:00:00Z
        let c1 = dir.join(".gemini/antigravity-cli/brain/conv-1/.system_generated/logs");
        let c2 = dir.join(".gemini/antigravity-cli/brain/conv-2/.system_generated/logs");
        fs::create_dir_all(&c1).unwrap();
        fs::create_dir_all(&c2).unwrap();

        let lines1 = [
            // Yesterday: excluded
            json!({"step_index": 0, "type": "PLANNER_RESPONSE", "created_at": "2025-11-19T23:59:59Z", "input_tokens": 999, "output_tokens": 999}).to_string(),
            // Mentioning "PLANNER_RESPONSE" inside a GENERIC step: ignored
            json!({"step_index": 1, "type": "GENERIC", "created_at": "2025-11-20T01:00:00Z", "content": "\"PLANNER_RESPONSE\"", "input_tokens": 888}).to_string(),
            // PLANNER_RESPONSE without any token fields: ignored
            json!({"step_index": 2, "type": "PLANNER_RESPONSE", "created_at": "2025-11-20T01:00:01Z", "status": "ERROR"}).to_string(),
            // First step_index=3 wins
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "created_at": "2025-11-20T01:00:02Z", "input_tokens": 100, "output_tokens": 40, "cache_creation_tokens": 10, "cache_read_tokens": 500}).to_string(),
            // Duplicate step_index=3 in same file: deduped
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "created_at": "2025-11-20T01:00:03Z", "input_tokens": 999, "output_tokens": 999}).to_string(),
            // No step_index: never deduped
            json!({"type": "PLANNER_RESPONSE", "created_at": "2025-11-20T01:00:04Z", "input_tokens": 5, "output_tokens": 2}).to_string(),
            json!({"type": "PLANNER_RESPONSE", "created_at": "2025-11-20T01:00:05Z", "input_tokens": 5, "output_tokens": 2}).to_string(),
        ];
        fs::write(c1.join("transcript.jsonl"), lines1.join("\n") + "\n").unwrap();
        // transcript_full.jsonl MUST be ignored
        fs::write(
            c1.join("transcript_full.jsonl"),
            json!({"step_index": 99, "type": "PLANNER_RESPONSE", "created_at": "2025-11-20T01:00:02Z", "input_tokens": 50000, "output_tokens": 50000}).to_string() + "\n",
        )
        .unwrap();

        // Same step_index=3 in a different conversation file MUST NOT collide
        fs::write(
            c2.join("transcript.jsonl"),
            json!({"step_index": 3, "type": "PLANNER_RESPONSE", "created_at": "2025-11-20T02:00:00Z", "input_tokens": 200, "output_tokens": 60, "cache_read_tokens": 300}).to_string() + "\n",
        )
        .unwrap();

        let r1 = gemini_tokens_today(&dir, midnight);
        assert!(r1.trace.contains("cache cold"), "{}", r1.trace);
        assert_eq!(r1.value["today_uncached_input"], 310);
        assert_eq!(r1.value["today_output"], 104);
        assert_eq!(r1.value["today_total"], 310 + 104 + 10 + 800);
        assert!(dir.join(".cache/agent-usage/gemini-tokens-v1.bin").exists());

        // Warm incremental append
        let mut f = fs::OpenOptions::new().append(true).open(c2.join("transcript.jsonl")).unwrap();
        writeln!(
            f,
            "{}",
            json!({"step_index": 4, "type": "PLANNER_RESPONSE", "created_at": "2025-11-20T03:00:00Z", "input_tokens": 20, "output_tokens": 6})
        )
        .unwrap();
        let r2 = gemini_tokens_today(&dir, midnight);
        assert!(r2.trace.contains("cache warm"), "{}", r2.trace);
        assert_eq!(r2.value["today_uncached_input"], 330);
        assert_eq!(r2.value["today_output"], 110);

        fs::remove_dir_all(&dir).unwrap();
    }
}
