//! The .xz container around LZMA2 data: stream header, one block, index, footer,
//! with CRC32/CRC64/SHA-256 integrity checks. Reads anything plain-LZMA2 that the
//! xz tool writes (no BCJ/Delta filters), writes files the xz tool reads.

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::lzma::Stats;
use crate::lzma2::{self, Lzma2Decoder, Progress};
use crate::settings::Settings;

const HEADER_MAGIC: &[u8; 6] = b"\xFD7zXZ\x00";
const FOOTER_MAGIC: &[u8; 2] = b"YZ";
const FILTER_LZMA2: u64 = 0x21;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    None = 0x00,
    Crc32 = 0x01,
    Crc64 = 0x04,
    Sha256 = 0x0A,
}

impl Check {
    fn from_id(id: u8) -> Option<Check> {
        match id {
            0x00 => Some(Check::None),
            0x01 => Some(Check::Crc32),
            0x04 => Some(Check::Crc64),
            0x0A => Some(Check::Sha256),
            _ => None,
        }
    }

    fn size_of_id(id: u8) -> usize {
        if id == 0 {
            0
        } else {
            4 << ((id - 1) / 3)
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Check::None => "none",
            Check::Crc32 => "CRC32",
            Check::Crc64 => "CRC64",
            Check::Sha256 => "SHA-256",
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

const CRC64_POLY: u64 = 0xC96C_5795_D787_0F42;

fn crc64_table() -> [u64; 256] {
    let mut t = [0u64; 256];
    for (i, slot) in t.iter_mut().enumerate() {
        let mut c = i as u64;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ CRC64_POLY
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    t
}

pub fn crc64(data: &[u8]) -> u64 {
    static TABLE: std::sync::LazyLock<[u64; 256]> = std::sync::LazyLock::new(crc64_table);
    let mut crc = u64::MAX;
    for &b in data {
        crc = TABLE[((crc ^ b as u64) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ u64::MAX
}

fn compute_check(id: u8, data: &[u8]) -> Option<Vec<u8>> {
    match Check::from_id(id)? {
        Check::None => Some(Vec::new()),
        Check::Crc32 => Some(crc32(data).to_le_bytes().to_vec()),
        Check::Crc64 => Some(crc64(data).to_le_bytes().to_vec()),
        Check::Sha256 => Some(Sha256::digest(data).to_vec()),
    }
}

fn encode_varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while v >= 0x80 {
        out.push((v & 0x7F) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out
}

fn decode_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for i in 0..9 {
        let b = *buf
            .get(*pos)
            .ok_or_else(|| Error::Corrupt("truncated integer".into()))?;
        *pos += 1;
        value |= ((b & 0x7F) as u64) << (7 * i);
        if b < 0x80 {
            if b == 0 && i > 0 {
                return Err(Error::Corrupt("badly encoded integer".into()));
            }
            return Ok(value);
        }
    }
    Err(Error::Corrupt("integer too long".into()))
}

fn pad4(n: usize) -> usize {
    (4 - n % 4) % 4
}

fn dict_size_to_prop(size: u32) -> u8 {
    for b in 0..40u8 {
        if prop_to_dict_size(b) >= size {
            return b;
        }
    }
    40
}

fn prop_to_dict_size(b: u8) -> u32 {
    if b == 40 {
        u32::MAX
    } else {
        (2 | (b as u32 & 1)) << (b / 2 + 11)
    }
}

fn block_header(compressed: usize, uncompressed: usize, dict_prop: u8) -> Vec<u8> {
    let mut body = vec![0x40 | 0x80]; // both sizes present, one filter
    body.extend(encode_varint(compressed as u64));
    body.extend(encode_varint(uncompressed as u64));
    body.extend(encode_varint(FILTER_LZMA2));
    body.extend(encode_varint(1));
    body.push(dict_prop);
    let mut size = 1 + body.len() + 4;
    size += pad4(size);
    let mut header = vec![(size / 4 - 1) as u8];
    header.extend(&body);
    header.resize(size - 4, 0);
    let crc = crc32(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    header
}

/// Build a complete .xz file. Returns (bytes, settings used, stats).
pub fn compress(
    data: &[u8],
    settings: &Settings,
    check: Check,
    progress: Option<Progress>,
) -> Result<(Vec<u8>, Settings, Stats)> {
    let flags = [0u8, check as u8];
    let mut out = Vec::new();
    out.extend_from_slice(HEADER_MAGIC);
    out.extend_from_slice(&flags);
    out.extend_from_slice(&crc32(&flags).to_le_bytes());
    let mut records: Vec<(usize, usize)> = Vec::new();
    let mut used = settings.clone();
    let mut stats = Stats::default();
    if !data.is_empty() {
        let (stream, s, st) = lzma2::encode(data, settings, progress)?;
        used = s;
        stats = st;
        let header = block_header(
            stream.len(),
            data.len(),
            dict_size_to_prop(settings.dict_size),
        );
        let check_bytes = compute_check(check as u8, data).unwrap_or_default();
        out.extend_from_slice(&header);
        out.extend_from_slice(&stream);
        out.resize(out.len() + pad4(stream.len()), 0);
        out.extend_from_slice(&check_bytes);
        records.push((header.len() + stream.len() + check_bytes.len(), data.len()));
    }
    // index
    let mut index = vec![0u8];
    index.extend(encode_varint(records.len() as u64));
    for (unpadded, uncompressed) in &records {
        index.extend(encode_varint(*unpadded as u64));
        index.extend(encode_varint(*uncompressed as u64));
    }
    index.resize(index.len() + pad4(index.len()), 0);
    let icrc = crc32(&index);
    index.extend_from_slice(&icrc.to_le_bytes());
    out.extend_from_slice(&index);
    // footer
    let mut backward = ((index.len() / 4 - 1) as u32).to_le_bytes().to_vec();
    backward.extend_from_slice(&flags);
    out.extend_from_slice(&crc32(&backward).to_le_bytes());
    out.extend_from_slice(&backward);
    out.extend_from_slice(FOOTER_MAGIC);
    Ok((out, used, stats))
}

pub struct XzInfo {
    pub check: Check,
    pub blocks: usize,
    pub props: Option<(u32, u32, u32)>,
    pub dict_size: u32,
}

/// Decompress a complete .xz file, verifying every CRC and check. `max_output` caps
/// the total output (a decompression bomb is refused before it is built).
pub fn decompress(blob: &[u8], max_output: usize) -> Result<(Vec<u8>, XzInfo)> {
    let mut out = Vec::new();
    let mut info = XzInfo {
        check: Check::None,
        blocks: 0,
        props: None,
        dict_size: 0,
    };
    let mut pos = 0;
    let mut streams = 0;
    while pos < blob.len() {
        pos = decode_stream(blob, pos, &mut out, &mut info, max_output)?;
        streams += 1;
        let pad_start = pos;
        while pos < blob.len() && blob[pos] == 0 {
            pos += 1;
        }
        if (pos - pad_start) % 4 != 0 {
            return Err(Error::Corrupt(
                "stream padding is not a multiple of 4 bytes".into(),
            ));
        }
    }
    if streams == 0 {
        return Err(Error::Corrupt("empty file".into()));
    }
    Ok((out, info))
}

fn corrupt<T>(msg: &str) -> Result<T> {
    Err(Error::Corrupt(msg.to_string()))
}

fn decode_stream(
    blob: &[u8],
    mut pos: usize,
    out: &mut Vec<u8>,
    info: &mut XzInfo,
    max_output: usize,
) -> Result<usize> {
    let hdr = blob
        .get(pos..pos + 12)
        .ok_or_else(|| Error::Corrupt("truncated stream header".into()))?;
    if &hdr[..6] != HEADER_MAGIC {
        return corrupt("not an .xz stream (bad magic)");
    }
    let flags = &hdr[6..8];
    if crc32(flags) != u32::from_le_bytes(hdr[8..12].try_into().unwrap()) {
        return corrupt("stream header CRC32 mismatch");
    }
    if flags[0] != 0 || flags[1] & 0xF0 != 0 {
        return corrupt("unsupported stream flags");
    }
    let check_id = flags[1];
    info.check = Check::from_id(check_id).unwrap_or(Check::None);
    let check_size = Check::size_of_id(check_id);
    pos += 12;
    let mut records: Vec<(usize, usize)> = Vec::new();
    loop {
        let b = *blob
            .get(pos)
            .ok_or_else(|| Error::Corrupt("file ends inside a stream".into()))?;
        if b == 0 {
            break;
        }
        // block header
        let hsize = (b as usize + 1) * 4;
        let header = blob
            .get(pos..pos + hsize)
            .ok_or_else(|| Error::Corrupt("truncated block header".into()))?;
        if crc32(&header[..hsize - 4])
            != u32::from_le_bytes(header[hsize - 4..].try_into().unwrap())
        {
            return corrupt("block header CRC32 mismatch");
        }
        let bflags = header[1];
        if bflags & 0x3C != 0 {
            return corrupt("unsupported block header flags");
        }
        let mut p = 2;
        let end = hsize - 4;
        let comp = if bflags & 0x40 != 0 {
            Some(decode_varint(&header[..end], &mut p)?)
        } else {
            None
        };
        let uncomp = if bflags & 0x80 != 0 {
            Some(decode_varint(&header[..end], &mut p)?)
        } else {
            None
        };
        let nfilters = (bflags & 3) as usize + 1;
        if nfilters != 1 {
            return Err(Error::Corrupt(
                "only plain LZMA2 is supported (no BCJ/Delta filters)".into(),
            ));
        }
        let fid = decode_varint(&header[..end], &mut p)?;
        let psize = decode_varint(&header[..end], &mut p)? as usize;
        if fid != FILTER_LZMA2 || psize != 1 || p + 1 > end {
            return Err(Error::Corrupt(
                "unsupported filter chain (only plain LZMA2 is supported)".into(),
            ));
        }
        let dict_prop = header[p];
        p += 1;
        if dict_prop > 40 {
            return corrupt("invalid LZMA2 dictionary size");
        }
        if header[p..end].iter().any(|&x| x != 0) {
            return corrupt("bad block header padding");
        }
        let dict_size = prop_to_dict_size(dict_prop);
        info.dict_size = dict_size;
        pos += hsize;
        let data_start = pos;
        // LZMA2 data
        let size_hint = uncomp
            .map(|u| u.min(max_output as u64) as usize)
            .unwrap_or(0);
        let limit = match uncomp {
            Some(u) if u as usize > max_output => {
                return Err(Error::Archive(format!(
                    "block would expand to {u} bytes, above the limit"
                )))
            }
            Some(u) => u as usize,
            None => max_output,
        };
        let before = out.len();
        let mut dec = Lzma2Decoder::new(dict_size, size_hint);
        pos = dec.decode(blob, pos, limit)?;
        let block_out = dec.dec.out;
        info.props = dec.props.or(info.props);
        let comp_size = pos - data_start;
        if comp.is_some_and(|c| c as usize != comp_size) {
            return corrupt("block compressed size does not match its header");
        }
        if uncomp.is_some_and(|u| u as usize != block_out.len()) {
            return corrupt("block uncompressed size does not match its header");
        }
        let pad = pad4(comp_size);
        if blob
            .get(pos..pos + pad)
            .is_none_or(|p| p.iter().any(|&x| x != 0))
        {
            return corrupt("non-zero block padding");
        }
        pos += pad;
        let stored = blob
            .get(pos..pos + check_size)
            .ok_or_else(|| Error::Corrupt("truncated block check".into()))?;
        if let Some(expected) = compute_check(check_id, &block_out) {
            if stored != expected.as_slice() {
                return Err(Error::Corrupt(format!(
                    "{} mismatch: the decompressed data is corrupt",
                    info.check.name()
                )));
            }
        }
        pos += check_size;
        records.push((hsize + comp_size + check_size, block_out.len()));
        out.extend_from_slice(&block_out);
        let _ = before;
        info.blocks += 1;
    }
    // index
    let index_start = pos;
    pos += 1;
    let count = decode_varint(blob, &mut pos)?;
    if count as usize != records.len() {
        return corrupt("index block count does not match the file");
    }
    for (unpadded, uncompressed) in &records {
        let u = decode_varint(blob, &mut pos)?;
        let c = decode_varint(blob, &mut pos)?;
        if u != *unpadded as u64 || c != *uncompressed as u64 {
            return corrupt("index sizes do not match the blocks");
        }
    }
    let pad = pad4(pos - index_start);
    if blob
        .get(pos..pos + pad)
        .is_none_or(|p| p.iter().any(|&x| x != 0))
    {
        return corrupt("non-zero index padding");
    }
    pos += pad;
    let icrc = blob
        .get(pos..pos + 4)
        .ok_or_else(|| Error::Corrupt("truncated index".into()))?;
    if crc32(&blob[index_start..pos]) != u32::from_le_bytes(icrc.try_into().unwrap()) {
        return corrupt("index CRC32 mismatch");
    }
    pos += 4;
    let index_size = pos - index_start;
    // footer
    let footer = blob
        .get(pos..pos + 12)
        .ok_or_else(|| Error::Corrupt("missing stream footer".into()))?;
    if &footer[10..12] != FOOTER_MAGIC {
        return corrupt("missing stream footer");
    }
    if crc32(&footer[4..10]) != u32::from_le_bytes(footer[0..4].try_into().unwrap()) {
        return corrupt("stream footer CRC32 mismatch");
    }
    if (u32::from_le_bytes(footer[4..8].try_into().unwrap()) as usize + 1) * 4 != index_size {
        return corrupt("stream footer index size mismatch");
    }
    if &footer[8..10] != flags {
        return corrupt("stream header and footer flags differ");
    }
    Ok(pos + 12)
}
