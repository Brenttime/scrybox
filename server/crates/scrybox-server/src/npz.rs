//! npz.js twin: a minimal .npz reader — a ZIP of .npy members, STORED or
//! DEFLATE (numpy writes savez / savez_compressed respectively). Ported
//! line-for-line from backend/src/utils/npz.js; the milo catalog is the only
//! consumer (milo-{game}.npz: embeddings f4 [n,dim], card_ids <U41 [n]).

use flate2::bufread::DeflateDecoder;
use std::collections::HashMap;
use std::fs;
use std::io::Read;

pub struct Npy {
    pub shape: Vec<usize>,
    pub descr: String,
    pub fortran: bool,
    pub data: NpyData,
}

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
