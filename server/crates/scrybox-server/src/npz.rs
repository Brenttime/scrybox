//! npz.js twin: a minimal .npz reader — a ZIP of .npy members, STORED or
//! DEFLATE (numpy writes savez / savez_compressed respectively). Ported
//! line-for-line from backend/src/utils/npz.js; the milo catalog is the only
//! consumer (milo-{game}.npz: embeddings f4 [n,dim], card_ids <U41 [n]).

use flate2::bufread::DeflateDecoder;
use std::collections::HashMap;
use std::fs;
use std::io::Read;

#[derive(Debug)]
pub struct Npy {
    pub shape: Vec<usize>,
    pub descr: String,
    pub fortran: bool,
    pub data: NpyData,
}

#[derive(Debug)]
pub enum NpyData {
    F32(Vec<f32>),
    F64(Vec<f64>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    I8(Vec<i8>),
    U8(Vec<u8>),
    U32(Vec<u32>),
    /// <U / |S fixed-width members decoded to owned strings.
    Str(Vec<String>),
}

pub struct Entry {
    name: String,
    method: u16,
    comp_size: usize,
    local_off: usize,
}

fn u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

fn read_zip_entries(buf: &[u8]) -> Result<Vec<Entry>, String> {
    // EOCD: scan back over the max 64k comment.
    if buf.len() < 22 {
        return Err("not a zip: no EOCD".into());
    }
    let mut eocd: Option<usize> = None;
    let lo = buf.len().saturating_sub(65558);
    let mut i = buf.len() - 22;
    loop {
        if u32le(buf, i) == 0x0605_4b50 {
            eocd = Some(i);
            break;
        }
        if i <= lo {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.ok_or("not a zip: no EOCD")?;
    let mut count = u16(buf, eocd + 10) as usize;
    let mut off = u32le(buf, eocd + 16) as usize;
    // Zip64 sentinels (a catalog past 4 GB or 65k members).
    if off == 0xffff_ffff || count == 0xffff {
        let mut z: Option<usize> = None;
        if eocd >= 20 {
            let mut j = eocd - 20;
            loop {
                if u32le(buf, j) == 0x0706_4b50 {
                    z = Some(j);
                    break;
                }
                if j == 0 {
                    break;
                }
                j -= 1;
            }
        }
        let z = z.ok_or("zip64 sentinel but no locator")?;
        let z64 = u64le(buf, z + 8) as usize;
        count = u64le(buf, z64 + 32) as usize;
        off = u64le(buf, z64 + 48) as usize;
    }
    let mut out = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        if u32le(buf, off) != 0x0201_4b50 {
            return Err("bad central header".into());
        }
        let method = u16(buf, off + 10);
        let comp_size = u32le(buf, off + 20) as usize;
        let name_len = u16(buf, off + 28) as usize;
        let extra_len = u16(buf, off + 30) as usize;
        let comment_len = u16(buf, off + 32) as usize;
        let local_off = u32le(buf, off + 42) as usize;
        let name = String::from_utf8_lossy(&buf[off + 46..off + 46 + name_len]).into_owned();
        out.push(Entry { name, method, comp_size, local_off });
        off += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// Length-bounded Read over a seeked File: lets inflate stream straight from
/// the npz on disk instead of materialising the whole file first (the 56 MB
/// milo catalog npz used to sit in memory beside its decompressed members —
/// iter-42 memory queue item 1).
struct BoundedRead {
    f: std::fs::File,
    remaining: u64,
}

impl std::io::Read for BoundedRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let take = std::cmp::min(self.remaining, buf.len() as u64) as usize;
        let got = self.f.read(&mut buf[..take])?;
        self.remaining -= got as u64;
        Ok(got)
    }
}

fn inflate_read(r: impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut d = flate2::read::DeflateDecoder::new(r);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    Ok(out)
}

/// Central-directory entries needed to stream one member from disk.
struct CdEntry {
    name: String,
    compress: u16,
    header_offset: u64,
    csize: u64,
}

/// Read the End-Of-Central-Directory record from the file tail; returns the
/// absolute EOCD position and the member count.
fn read_eocd(file: &mut std::fs::File) -> Result<(u64, u64, u64), String> {
    use std::io::Seek;
    let end = file.metadata().map_err(|e| e.to_string())?.len();
    let win = std::cmp::min(end, 66_000);
    let win_start = end - win;
    file.seek(std::io::SeekFrom::Start(win_start)).map_err(|e| e.to_string())?;
    let mut tail = Vec::with_capacity(win as usize);
    file.read_to_end(&mut tail).map_err(|e| e.to_string())?;
    let mut p = None;
    if tail.len() >= 22 {
        for i in (0..=tail.len() - 22).rev() {
            if &tail[i..i + 4] == b"PK\x05\x06" {
                p = Some(i);
                break;
            }
        }
    }
    let p = p.ok_or("npz EOCD")?;
    if &tail[p + 16..p + 20] == b"PK\x06\x07" || u32::from_le_bytes([tail[p + 16], tail[p + 17], tail[p + 18], tail[p + 19]]) == 0xFFFFFFFF {
        return Err("npz zip64 EOCD locator unsupported".into());
    }
    let count = u16::from_le_bytes([tail[p + 10], tail[p + 11]]) as u64;
    let cd_off = u32::from_le_bytes([tail[p + 16], tail[p + 17], tail[p + 18], tail[p + 19]]) as u64;
    Ok((cd_off, win_start + p as u64, count))
}

fn read_cd(file: &mut std::fs::File) -> Result<Vec<CdEntry>, String> {
    use std::io::Seek;
    let (cd_off, eocd_pos, count) = read_eocd(file)?;
    // The central directory sits exactly between cd_off and the EOCD.
    let cd_start = cd_off;
    file.seek(std::io::SeekFrom::Start(cd_start)).map_err(|e| e.to_string())?;
    let mut cd = Vec::with_capacity((eocd_pos - cd_start) as usize);
    file.take((eocd_pos - cd_start) as u64).read_to_end(&mut cd).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    let mut i = 0usize;
    let rd16 = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let rd32 = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    for _ in 0..count {
        if i + 46 > cd.len() || &cd[i..i + 4] != b"PK\x01\x02" {
            break;
        }
        let compress = rd16(&cd, i + 10);
        let csize = rd32(&cd, i + 20) as u64;
        let name_len = rd16(&cd, i + 28) as usize;
        let extra_len = rd16(&cd, i + 30) as usize;
        let comment_len = rd16(&cd, i + 32) as usize;
        let ho = rd32(&cd, i + 42) as u64;
        let name = String::from_utf8_lossy(&cd[i + 46..i + 46 + name_len]).to_string();
        entries.push(CdEntry { name, compress, header_offset: ho, csize });
        i += 46 + name_len + extra_len + comment_len;
    }
    Ok(entries)
}

/// Read one member's raw stored bytes streaming from the file (local header
/// at `header_offset` gives the data start; the central directory csize gives
/// the compressed length).
fn member_raw(file: &mut std::fs::File, e: &CdEntry) -> Result<Vec<u8>, String> {
    use std::io::Seek;
    let mut lh = [0u8; 30];
    file.seek(std::io::SeekFrom::Start(e.header_offset)).map_err(|err| err.to_string())?;
    file.read_exact(&mut lh).map_err(|err| err.to_string())?;
    if &lh[0..4] != b"PK\x03\x04" {
        return Err(format!("zip local header {}", e.name));
    }
    let name_len = u16::from_le_bytes([lh[26], lh[27]]) as u64;
    let extra_len = u16::from_le_bytes([lh[28], lh[29]]) as u64;
    let data_off = e.header_offset + 30 + name_len + extra_len;
    file.seek(std::io::SeekFrom::Start(data_off)).map_err(|err| err.to_string())?;
    let br = BoundedRead { f: file.try_clone().map_err(|err| err.to_string())?, remaining: e.csize };
    match e.compress {
        0 => {
            let mut r = br;
            let mut raw = Vec::new();
            r.read_to_end(&mut raw).map_err(|err| err.to_string())?;
            Ok(raw)
        }
        8 => inflate_read(br).map_err(|err| format!("zip inflate {}: {err}", e.name)),
        _ => Err(format!("zip method {}", e.compress)),
    }
}

/// Parse ONLY the wanted members, streaming each straight off disk: no
/// whole-file buffer, no members kept that nobody asked for. Used by the
/// scan catalog (embeddings + card_ids) — the npz is 56 MB compressed and
/// loading every member (incl. the ~7 MB oracle_ids array nobody reads) plus
/// the whole-file buffer was the iter-42 VmHWM hotspot. Same on-disk format
/// and per-member error strings as load_npz.
pub fn load_npz_members(file: &str, wanted: &[&str]) -> Result<HashMap<String, Result<Npy, String>>, String> {
    let mut f = std::fs::File::open(file).map_err(|e| format!("read {file}: {e}"))?;
    let entries = read_cd(&mut f)?;
    let mut members: HashMap<String, Result<Npy, String>> = HashMap::new();
    for e in entries {
        let name = e.name.strip_suffix(".npy").unwrap_or(&e.name).to_string();
        if !wanted.contains(&name.as_str()) || members.contains_key(&name) {
            continue;
        }
        let raw = member_raw(&mut f, &e)?;
        if raw.is_empty() {
            members.insert(name, Err(format!("npz member '{}' is empty", e.name)));
            continue;
        }
        match parse_npy(&raw) {
            Ok(d) => {
                members.insert(name, Ok(d));
            }
            Err(err) => {
                members.insert(name, Err(err));
            }
        }
    }
    Ok(members)
}

fn entry_data(buf: &[u8], e: &Entry) -> Result<Vec<u8>, String> {
    // Local header repeats name/extra lengths — re-read, never reuse central.
    let lname_len = u16(buf, e.local_off + 26) as usize;
    let lextra_len = u16(buf, e.local_off + 28) as usize;
    let start = e.local_off + 30 + lname_len + lextra_len;
    let raw = buf
        .get(start..start + e.comp_size)
        .ok_or("entry extends past EOF")?;
    if e.method == 0 {
        return Ok(raw.to_vec());
    }
    let mut dec = DeflateDecoder::new(raw);
    let mut out = Vec::with_capacity(e.comp_size * 4);
    dec.read_to_end(&mut out).map_err(|e| format!("inflate: {e}"))?;
    Ok(out)
}

fn parse_f32(body: &[u8], n: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(n);
    for chunk in body.chunks_exact(4) {
        v.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        if v.len() == n {
            break;
        }
    }
    v
}

fn parse_npy(b: &[u8]) -> Result<Npy, String> {
    if b.len() < 10 || &b[0..6] != b"\x93NUMPY" {
        return Err("not npy".into());
    }
    let major = b[6];
    let (hlen, hstart) = if major == 1 {
        (u16(b, 8) as usize, 10usize)
    } else {
        (u32le(b, 8) as usize, 12usize)
    };
    let header = String::from_utf8_lossy(&b[hstart..hstart + hlen]).into_owned();
    let descr = regex_capture(&header, r"'descr':\s*'([^']+)'").ok_or("no descr")?;
    let fortran = regex_capture(&header, r"'fortran_order':\s*(True|False)")
        .map(|s| s == "True")
        .unwrap_or(false);
    let shape_str = regex_capture(&header, r"'shape':\s*\(([^)]*)\)").unwrap_or_default();
    let shape: Vec<usize> = split_digits(&shape_str);
    let body = &b[hstart + hlen..];
    let n: usize = shape.iter().product();

    if descr.starts_with("<U") {
        // Fixed-width UTF-32LE strings: cut at the first NUL codepoint.
        let w: usize = descr[2..].parse().map_err(|_| "bad <U width")?;
        let mut arr = Vec::with_capacity(n);
        for i in 0..n {
            let mut s = String::new();
            for j in 0..w {
                let o = (i * w + j) * 4;
                let cp = u32le(body, o);
                if cp == 0 {
                    break;
                }
                s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
            }
            arr.push(s);
        }
        return Ok(Npy { shape, descr, fortran, data: NpyData::Str(arr) });
    }
    if descr.starts_with("|S") {
        let w: usize = descr[2..].parse().map_err(|_| "bad |S width")?;
        let mut arr = Vec::with_capacity(n);
        for i in 0..n {
            let s = &body[i * w..i * w + w];
            let end = s.iter().position(|c| *c == 0).unwrap_or(w);
            arr.push(String::from_utf8_lossy(&s[..end]).into_owned());
        }
        return Ok(Npy { shape, descr, fortran, data: NpyData::Str(arr) });
    }
    if descr == "|O" {
        return Err("pickled object array (needs allow_pickle)".into());
    }
    let data = match descr.as_str() {
        "<f4" => NpyData::F32(parse_f32(body, n)),
        "<f8" => {
            let mut v = Vec::with_capacity(n);
            for c in body.chunks_exact(8).take(n) {
                v.push(f64::from_le_bytes(c.try_into().unwrap()));
            }
            NpyData::F64(v)
        }
        "<i4" => {
            let mut v = Vec::with_capacity(n);
            for c in body.chunks_exact(4).take(n) {
                v.push(i32::from_le_bytes(c.try_into().unwrap()));
            }
            NpyData::I32(v)
        }
        "<i8" => {
            let mut v = Vec::with_capacity(n);
            for c in body.chunks_exact(8).take(n) {
                v.push(i64::from_le_bytes(c.try_into().unwrap()));
            }
            NpyData::I64(v)
        }
        "|i1" => NpyData::I8(body.iter().take(n).map(|b| *b as i8).collect()),
        "|u1" => NpyData::U8(body.iter().take(n).copied().collect()),
        "<u4" => {
            let mut v = Vec::with_capacity(n);
            for c in body.chunks_exact(4).take(n) {
                v.push(u32::from_le_bytes(c.try_into().unwrap()));
            }
            NpyData::U32(v)
        }
        other => return Err(format!("unsupported dtype {other}")),
    };
    Ok(Npy { shape, descr, fortran, data })
}

fn regex_capture(hay: &str, pat: &str) -> Option<String> {
    let re = regex::Regex::new(pat).ok()?;
    re.captures(hay).and_then(|c| c.get(1)).map(|m| m.as_str().to_string())
}

fn split_digits(s: &str) -> Vec<usize> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect()
}

/// loadNpz twin: every member parsed on a best-effort basis; a member that
/// fails carries its error message instead of poisoning the whole archive.
/// Callers pattern-match NpyData and treat members as present-or-error.
pub fn load_npz(file: &str) -> Result<HashMap<String, Result<Npy, String>>, String> {
    let buf = fs::read(file).map_err(|e| format!("read {file}: {e}"))?;
    let entries = read_zip_entries(&buf)?;
    let mut out = HashMap::new();
    for e in entries {
        let key = e.name.strip_suffix(".npy").unwrap_or(&e.name).to_string();
        let parsed = entry_data(&buf, &e).and_then(|raw| parse_npy(&raw));
        out.insert(key, parsed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = 0u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xEDB88320 & mask);
            }
        }
        !crc
    }

    /// Minimal zip (npz) writer: STORED (method 0, what np.savez emits) or
    /// DEFLATE (method 8, what the shipped milo catalog uses).
    fn zip_build(entries: &[(&str, &[u8], u16)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, data, method) in entries {
            let off = out.len() as u32;
            let comp = if *method == 8 {
                let mut e = flate2::write::DeflateEncoder::new(
                    Vec::new(),
                    flate2::Compression::default(),
                );
                use std::io::Write;
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]);
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0u8; 4]); // time/date
            out.extend_from_slice(&crc32(data).to_le_bytes());
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&comp);
            central.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]);
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0u8; 4]);
            central.extend_from_slice(&crc32(data).to_le_bytes());
            central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0u8; 12]);
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn npy(descr: &str, shape: &str, payload: &[u8]) -> Vec<u8> {
        let mut h = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': {shape}, }}");
        while (10 + 2 + h.len() + 1) % 64 != 0 {
            h.push(' ');
        }
        h.push('\n');
        let mut b = vec![0x93, b'N', b'U', b'M', b'P', b'Y', 1, 0];
        b.extend_from_slice(&(h.len() as u16).to_le_bytes());
        b.extend_from_slice(h.as_bytes());
        b.extend_from_slice(payload);
        b
    }

    fn memchr_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        if needle.is_empty() || hay.len() < needle.len() {
            return out;
        }
        for i in 0..=hay.len() - needle.len() {
            if &hay[i..i + needle.len()] == needle {
                out.push(i);
            }
        }
        out
    }

    fn tmp_npz(tag: &str, bytes: &[u8]) -> String {
        let p = std::env::temp_dir().join(format!(
            "scrybox-npz-{tag}-{}-{:?}.npz",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&p, bytes).unwrap();
        p.to_string_lossy().to_string()
    }

    #[test]
    fn load_npz_members_streams_wanted_only_and_matches_load_npz() {
        // STORED embeddings, DEFLATE ids (like milo-mtg.npz), plus a
        // deliberately-corrupt third member nobody asked for: load_npz fails
        // the whole file only when the corrupt member is REQUESTED — with
        // wanted-only parsing it never even gets decoded.
        let emb = npy(
            "<f4",
            "(2, 2)",
            &[1.5f32.to_le_bytes(), (-2.25f32).to_le_bytes(), 0.0f32.to_le_bytes(), 7.0f32.to_le_bytes()].concat(),
        );
        // <U4 x2: "ab\0\0" and "c\0\0\0" in UTF-32LE, wrapped as a real .npy.
        let mut ids_raw = Vec::new();
        for ch in ["ab\0", "c\0"] {
            for c in ch.encode_utf16().chain(std::iter::once(0)) {
                ids_raw.extend_from_slice(&(c as u32).to_le_bytes());
            }
        }
        let ids = npy("<U4", "(2,)", &ids_raw);
        let file = tmp_npz(
            "ok",
            &zip_build(&[
                ("embeddings.npy", &emb, 0),
                ("card_ids.npy", &ids, 8),
                ("broken.npy", b"not-npy-bytes-at-all", 0),
            ]),
        );
        let got = load_npz_members(&file, &["embeddings", "card_ids"]).unwrap();
        assert!(!got.contains_key("broken"));
        match &got["embeddings"] {
            Ok(Npy { data: NpyData::F32(v), shape, .. }) => {
                assert_eq!(v, &vec![1.5, -2.25, 0.0, 7.0]);
                assert_eq!(shape, &vec![2, 2]);
            }
            other => panic!("bad embeddings: {other:?}"),
        }
        match &got["card_ids"] {
            Ok(Npy { data: NpyData::Str(v), .. }) => assert_eq!(v, &vec!["ab".to_string(), "c".to_string()]),
            other => panic!("bad ids: {other:?}"),
        }
        // load_npz (the whole-map twin) agrees on the requested members.
        let all = load_npz(&file).unwrap();
        assert!(matches!(all.get("broken"), Some(Err(_))));
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn load_npz_members_error_semantics() {
        let emb = npy("<f4", "(1,)", &1.0f32.to_le_bytes());
        // Missing member => simply absent (caller's ok_or string fires).
        let file = tmp_npz("missing", &zip_build(&[("embeddings.npy", &emb, 0)]));
        let got = load_npz_members(&file, &["embeddings", "card_ids"]).unwrap();
        assert!(got.contains_key("embeddings"));
        assert!(!got.contains_key("card_ids"));
        std::fs::remove_file(&file).ok();
        // Empty member => per-member Err with the load_npz string.
        let file = tmp_npz("empty", &zip_build(&[("card_ids.npy", &[], 0)]));
        let got = load_npz_members(&file, &["card_ids"]).unwrap();
        assert!(
            matches!(&got["card_ids"], Err(e) if e == "npz member 'card_ids.npy' is empty"),
            "{:?}",
            got["card_ids"]
        );
        std::fs::remove_file(&file).ok();
        // A corrupt member among wanted ones surfaces as an inflate error for
        // THAT member only: store bytes that are not a valid deflate stream
        // (0x07 => BFINAL=1, BTYPE=11 = invalid block type) and patch both
        // method fields to 8 afterwards (zip_build itself always compresses).
        let mut bytes = zip_build(&[
            ("embeddings.npy", &emb, 0),
            ("card_ids.npy", &[0x07u8; 8], 0),
        ]);
        for idx in memchr_all(&bytes, b"card_ids.npy") {
            // name sits directly after the 30-byte local header / 46-byte central record
            let (hdr, moff) = if idx >= 30 && bytes[idx - 30..idx - 26] == *b"PK\x03\x04" {
                (idx - 30, 8)
            } else if idx >= 46 && bytes[idx - 46..idx - 42] == *b"PK\x01\x02" {
                (idx - 46, 10)
            } else {
                continue;
            };
            bytes[hdr + moff..hdr + moff + 2].copy_from_slice(&8u16.to_le_bytes());
        }
        let file = tmp_npz("corrupt", &bytes);
        let got = load_npz_members(&file, &["embeddings", "card_ids"]);
        match got {
            Err(e) => assert!(e.starts_with("zip inflate card_ids.npy"), "{e}"),
            Ok(m) => panic!("expected inflate error, got {:?}", m.get("card_ids")),
        }
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn split_digits_shapes() {
        assert_eq!(split_digits("109711, 128, "), vec![109711usize, 128]);
        assert_eq!(split_digits("(4,)"), vec![4]);
        assert!(split_digits("").is_empty());
    }

    #[test]
    fn regex_header_fields() {
        let h = "{'descr': '<f4', 'fortran_order': False, 'shape': (3, 2), }";
        assert_eq!(regex_capture(h, r"'descr':\s*'([^']+)'").unwrap(), "<f4");
        assert_eq!(regex_capture(h, r"'fortran_order':\s*(True|False)").unwrap(), "False");
        assert_eq!(regex_capture(h, r"'shape':\s*\(([^)]*)\)").unwrap(), "3, 2");
    }

    #[test]
    fn parse_npy_f4_and_u_strings() {
        // Hand-build a v1 header npy: <f4, shape (2,), 4 bytes.
        let mut b: Vec<u8> = vec![0x93, b'N', b'U', b'M', b'P', b'Y', 1, 0];
        let header = "{'descr': '<f4', 'fortran_order': False, 'shape': (2,), }          ";
        b.extend_from_slice(&(header.len() as u16).to_le_bytes());
        b.extend_from_slice(header.as_bytes());
        b.extend_from_slice(&1.5f32.to_le_bytes());
        b.extend_from_slice(&(-2.25f32).to_le_bytes());
        let npy = parse_npy(&b).unwrap();
        assert_eq!(npy.shape, vec![2]);
        match npy.data {
            NpyData::F32(v) => assert_eq!(v, vec![1.5, -2.25]),
            _ => panic!("wrong dtype"),
        }
    }
}
