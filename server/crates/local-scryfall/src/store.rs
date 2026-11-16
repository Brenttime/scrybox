//! Compact storage for the index's two biggest tables.
//!
//! * [`StrArena`] — every interned string in ONE contiguous buffer plus an
//!   end-offset table (`strings[i]` -> `&str`). Replaces `Vec<Box<str>>`
//!   (16 B fat pointer + a malloc chunk per string, ~840k strings).
//! * [`JsonStore`] — the pre-serialized per-card JSON bodies, zstd-compressed
//!   one document per frame against a dictionary trained on the first
//!   [`TRAIN_DOCS`] documents. Decompression is byte-exact, so every route
//!   serves the identical bytes it served from the old `Vec<Box<str>>`.

use std::ops::Index as IndexOp;

#[derive(Default)]
pub struct StrArena {
    buf: String,
    /// ends[i] = end offset of string i in `buf` (start = ends[i-1] or 0).
    ends: Vec<u32>,
}

impl StrArena {
    pub fn with_capacity(n: usize, bytes: usize) -> Self {
        Self { buf: String::with_capacity(bytes), ends: Vec::with_capacity(n) }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.ends.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
    /// Append `s`; returns its id.
    pub fn push(&mut self, s: &str) -> u32 {
        self.buf.push_str(s);
        assert!(self.buf.len() <= u32::MAX as usize, "string arena > 4 GiB");
        self.ends.push(self.buf.len() as u32);
        (self.ends.len() - 1) as u32
    }
    #[inline]
    pub fn get(&self, i: usize) -> &str {
        let e = self.ends[i] as usize;
        let s = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        // SAFETY-free: slicing on pushed &str boundaries is always valid UTF-8.
        &self.buf[s..e]
    }
    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }
    pub fn shrink_to_fit(&mut self) {
        self.buf.shrink_to_fit();
        self.ends.shrink_to_fit();
    }
    pub fn heap_bytes(&self) -> usize {
        self.buf.capacity() + self.ends.capacity() * 4
    }
}

impl IndexOp<usize> for StrArena {
    type Output = str;
    #[inline]
    fn index(&self, i: usize) -> &str {
        self.get(i)
    }
}

/// Scryfall-id -> card index. Canonical lowercase hyphenated UUIDs (every
/// Scryfall id) live in a sorted `u128` table (binary search, 20 B/card vs
/// ~90 B for a `HashMap<Box<str>, u32>`); anything else falls back to an
/// exact-string map, so lookups keep exact string-equality semantics.
#[derive(Default)]
pub struct IdMap {
    keys: Vec<u128>,
    vals: Vec<u32>,
    other: std::collections::HashMap<Box<str>, u32>,
    pending: Vec<(u128, u32)>,
}

/// Parse a canonical (lowercase, 8-4-4-4-12) UUID; None for anything else.
pub fn uuid_key(s: &str) -> Option<u128> {
    let b = s.as_bytes();
    if b.len() != 36 {
        return None;
    }
    let mut v: u128 = 0;
    for (i, &c) in b.iter().enumerate() {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            if c != b'-' {
                return None;
            }
            continue;
        }
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u128;
    }
    Some(v)
}

impl IdMap {
    /// Insert (build phase; a later insert of the same id wins, like
    /// `HashMap::insert`). Call [`IdMap::finish`] before lookups.
    pub fn insert(&mut self, id: &str, idx: u32) {
        match uuid_key(id) {
            Some(k) => self.pending.push((k, idx)),
            None => {
                self.other.insert(id.into(), idx);
            }
        }
    }
    pub fn finish(&mut self) {
        let mut p = std::mem::take(&mut self.pending);
        // stable sort keeps insertion order among equal keys -> keep the last
        p.sort_by_key(|e| e.0);
        let mut keys = Vec::with_capacity(p.len());
        let mut vals = Vec::with_capacity(p.len());
        for (k, v) in p {
            if keys.last() == Some(&k) {
                *vals.last_mut().unwrap() = v;
            } else {
                keys.push(k);
                vals.push(v);
            }
        }
        keys.shrink_to_fit();
        vals.shrink_to_fit();
        self.keys = keys;
        self.vals = vals;
        self.other.shrink_to_fit();
    }
    pub fn get(&self, id: &str) -> Option<&u32> {
        match uuid_key(id) {
            Some(k) => self.keys.binary_search(&k).ok().map(|i| &self.vals[i]),
            None => self.other.get(id),
        }
    }
    pub fn len(&self) -> usize {
        self.keys.len() + self.other.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Documents buffered before the dictionary is trained.
pub const TRAIN_DOCS: usize = 4096;
const DICT_BYTES: usize = 112 * 1024;
const LEVEL: i32 = 3;
/// Batch size for the parallel compression after training.
const BATCH: usize = 2048;

pub struct JsonStore {
    blob: Vec<u8>,
    /// ends[i] = end offset of doc i's frame in `blob`.
    ends: Vec<u32>,
    /// uncompressed length of doc i (exact-size output buffer).
    raw_len: Vec<u32>,
    dict_raw: Vec<u8>,
    ddict: Option<zstd::dict::DecoderDictionary<'static>>,
    pending: Vec<String>,
}

impl Default for JsonStore {
    fn default() -> Self {
        Self { blob: Vec::new(), ends: Vec::new(), raw_len: Vec::new(), dict_raw: Vec::new(), ddict: None, pending: Vec::new() }
    }
}

pub struct JsonReader<'a> {
    st: &'a JsonStore,
    d: Option<zstd::bulk::Decompressor<'a>>,
}

impl JsonReader<'_> {
    /// Append document `i` to `out` (exact original bytes).
    pub fn append_to(&mut self, i: usize, out: &mut String) {
        let st = self.st;
        if i >= st.ends.len() {
            out.push_str(&st.pending[i - st.ends.len()]);
            return;
        }
        let s = if i == 0 { 0 } else { st.ends[i - 1] as usize };
        let frame = &st.blob[s..st.ends[i] as usize];
        let n = st.raw_len[i] as usize;
        if self.d.is_none() {
            let dict = st.ddict.as_ref().expect("json store not trained");
            self.d = Some(zstd::bulk::Decompressor::with_prepared_dictionary(dict).expect("zstd dctx"));
        }
        let d = self.d.as_mut().unwrap();
        let mut v = std::mem::take(out).into_bytes();
        let at = v.len();
        v.resize(at + n, 0);
        let got = d.decompress_to_buffer(frame, &mut v[at..]).expect("zstd decompress");
        assert_eq!(got, n, "json store length mismatch");
        // SAFETY: `v[..at]` came from a String and `v[at..]` is the exact
        // byte image of a pushed &str (zstd is lossless).
        *out = unsafe { String::from_utf8_unchecked(v) };
    }
}

impl JsonStore {
    pub fn with_capacity(n: usize) -> Self {
        let mut s = Self::default();
        s.ends.reserve_exact(n);
        s.raw_len.reserve_exact(n);
        s
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.ends.len() + self.pending.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Append one document (build phase). Compression happens in batches.
    pub fn push(&mut self, doc: String) {
        self.pending.push(doc);
        let threshold = if self.ddict.is_none() { TRAIN_DOCS } else { BATCH };
        if self.pending.len() >= threshold {
            self.flush();
        }
    }

    fn train(&mut self) {
        // Train on the buffered docs; tiny corpora (tests) fall back to a
        // raw-content dictionary (still byte-exact, just a worse ratio).
        let samples: Vec<&[u8]> = self.pending.iter().map(|s| s.as_bytes()).collect();
        let total: usize = samples.iter().map(|s| s.len()).sum();
        let dict = if samples.len() >= 64 && total >= DICT_BYTES * 4 {
            zstd::dict::from_samples(&samples, DICT_BYTES).unwrap_or_default()
        } else {
            Vec::new()
        };
        self.dict_raw = dict;
        self.ddict = Some(zstd::dict::DecoderDictionary::copy(&self.dict_raw));
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if self.ddict.is_none() {
            self.train();
        }
        use rayon::prelude::*;
        let dict = &self.dict_raw;
        let frames: Vec<Vec<u8>> = self
            .pending
            .par_chunks(256)
            .flat_map_iter(|chunk| {
                let mut c = zstd::bulk::Compressor::with_dictionary(LEVEL, dict).expect("zstd ctx");
                c.include_checksum(false).ok();
                c.include_contentsize(false).ok();
                c.include_dictid(false).ok();
                chunk.iter().map(move |d| c.compress(d.as_bytes()).expect("zstd compress")).collect::<Vec<_>>()
            })
            .collect();
        for (f, d) in frames.iter().zip(self.pending.iter()) {
            self.blob.extend_from_slice(f);
            assert!(self.blob.len() <= u32::MAX as usize, "json blob > 4 GiB");
            self.ends.push(self.blob.len() as u32);
            self.raw_len.push(d.len() as u32);
        }
        self.pending.clear();
    }

    /// End of the build: compress the tail and release slack.
    pub fn finish(&mut self) {
        self.flush();
        self.pending = Vec::new();
        self.blob.shrink_to_fit();
        self.ends.shrink_to_fit();
        self.raw_len.shrink_to_fit();
    }

    #[inline]
    pub fn raw_len(&self, i: usize) -> usize {
        if i >= self.ends.len() {
            return self.pending[i - self.ends.len()].len();
        }
        self.raw_len[i] as usize
    }

    /// A reader holding one decompression context (reuse it for every
    /// document of a response).
    pub fn reader(&self) -> JsonReader<'_> {
        JsonReader { st: self, d: None }
    }

    /// Append document `i` to `out` (exact original bytes).
    pub fn append_to(&self, i: usize, out: &mut String) {
        self.reader().append_to(i, out)
    }

    pub fn get(&self, i: usize) -> String {
        let mut s = String::with_capacity(self.raw_len(i));
        self.append_to(i, &mut s);
        s
    }

    pub fn heap_bytes(&self) -> usize {
        self.blob.capacity() + self.ends.capacity() * 4 + self.raw_len.capacity() * 4 + self.dict_raw.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn id_map_exact() {
        let mut m = IdMap::default();
        m.insert("0000579f-7b35-4ed3-b44c-db2a538066fe", 1);
        m.insert("ffffffff-ffff-ffff-ffff-ffffffffffff", 2);
        m.insert("0000579f-7b35-4ed3-b44c-db2a538066fe", 3); // later wins
        m.insert("NOT-A-UUID", 4);
        m.finish();
        assert_eq!(m.get("0000579f-7b35-4ed3-b44c-db2a538066fe"), Some(&3));
        assert_eq!(m.get("ffffffff-ffff-ffff-ffff-ffffffffffff"), Some(&2));
        assert_eq!(m.get("0000579F-7B35-4ED3-B44C-DB2A538066FE"), None);
        assert_eq!(m.get("0000579f7b354ed3b44cdb2a538066fe"), None);
        assert_eq!(m.get("NOT-A-UUID"), Some(&4));
        assert_eq!(m.get("not-a-uuid"), None);
        assert_eq!(m.len(), 3);
    }
    #[test]
    fn arena_roundtrip() {
        let mut a = StrArena::default();
        assert_eq!(a.push("abc"), 0);
        assert_eq!(a.push(""), 1);
        assert_eq!(a.push("héllo"), 2);
        assert_eq!(&a[0], "abc");
        assert_eq!(&a[1], "");
        assert_eq!(&a[2], "héllo");
        assert_eq!(a.iter().collect::<Vec<_>>(), vec!["abc", "", "héllo"]);
    }
    #[test]
    fn json_store_roundtrip_exact() {
        let docs: Vec<String> = (0..TRAIN_DOCS + 3 * BATCH + 17)
            .map(|i| format!("{{\"id\":\"{i:08x}\",\"name\":\"Card {i}\",\"legalities\":{{\"standard\":\"{}\"}},\"t\":\"{}\"}}", if i % 3 == 0 { "legal" } else { "not_legal" }, "x".repeat(i % 97)))
            .collect();
        let mut s = JsonStore::with_capacity(docs.len());
        for d in &docs {
            s.push(d.clone());
        }
        // readable mid-build (pending tail)
        assert_eq!(s.get(docs.len() - 1), docs[docs.len() - 1]);
        s.finish();
        assert_eq!(s.len(), docs.len());
        for (i, d) in docs.iter().enumerate() {
            assert_eq!(&s.get(i), d);
        }
        let mut out = String::from("[");
        s.append_to(3, &mut out);
        out.push(',');
        s.append_to(0, &mut out);
        assert_eq!(out, format!("[{},{}", docs[3], docs[0]));
        let tiny = {
            let mut t = JsonStore::default();
            t.push("{}".into());
            t.push("{\"a\":1}".into());
            t.finish();
            t
        };
        assert_eq!(tiny.get(0), "{}");
        assert_eq!(tiny.get(1), "{\"a\":1}");
    }
}
