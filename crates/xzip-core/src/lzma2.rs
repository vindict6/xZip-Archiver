//! LZMA2: LZMA in chunks (<= 2 MiB in, <= 64 KiB out). A chunk that would grow is
//! stored raw, and so is any window that looks incompressible before we even try.
//! Also the top-level encoder: dictionary-sized buffers, one shared radix table per
//! buffer, slices encoded in parallel, lc/lp/pb picked by trial.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::lzma::{parse_props_byte, props_byte, Decoder, Encoder, Matches, Model, Stats};
use crate::radix::RadixMatchFinder;
use crate::rangecoder::RangeDecoder;
use crate::settings::{Settings, Strategy, MATCH_LEN_MAX};

pub const MAX_UNPACKED: usize = 1 << 21;
pub const MAX_PACKED: usize = 1 << 16;
pub const MAX_STORED: usize = 1 << 16;
const CHUNK_SAFETY_MARGIN: usize = 64;
const SKIP_WINDOW: usize = MAX_STORED;
const SKIP_MIN_WINDOW: usize = 4096;
const SKIP_PROBE_SAMPLES: usize = 256;
const SKIP_MAX_MATCH_PERCENT: usize = 10;
const MIN_POOL_INPUT: usize = 1 << 16;

/// Called with the number of input bytes finished so far; return false to cancel.
pub type Progress<'p> = &'p (dyn Fn(u64) -> bool + Sync);

pub fn byte_entropy(sample: &[u8]) -> f64 {
    if sample.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for &b in sample {
        counts[b as usize] += 1;
    }
    let n = sample.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// Bytes at enc.pos not worth parsing: high entropy and the match finder knows no
/// matches for them (a copy of earlier data has high entropy too, but compresses).
fn incompressible_span<M: Matches>(enc: &Encoder<M>) -> usize {
    if !enc.s.skip_incompressible {
        return 0;
    }
    let pos = enc.pos;
    let window = (enc.end - pos).min(SKIP_WINDOW);
    if window < SKIP_MIN_WINDOW {
        return 0;
    }
    if byte_entropy(&enc.data[pos..pos + window]) < enc.s.skip_entropy {
        return 0;
    }
    let step = (window / SKIP_PROBE_SAMPLES).max(1);
    let samples: Vec<usize> = (pos..pos + window).step_by(step).collect();
    let hits = samples
        .iter()
        .filter(|&&p| enc.matches.probe(p) >= 4)
        .count();
    if hits * 100 > samples.len() * SKIP_MAX_MATCH_PERCENT {
        return 0;
    }
    window
}

fn encode_chunk<M: Matches>(
    enc: &mut Encoder<M>,
    progress: Option<Progress>,
) -> Result<(Vec<u8>, usize)> {
    enc.rc = crate::rangecoder::RangeEncoder::new();
    let start = enc.pos;
    let unpacked_limit = start + MAX_UNPACKED - MATCH_LEN_MAX as usize;
    let packed_limit = MAX_PACKED - CHUNK_SAFETY_MARGIN;
    let mut next_report = start + 16384;
    let mut next_probe = start;
    while enc.pos < enc.end && enc.pos <= unpacked_limit && enc.rc.pending_size() < packed_limit {
        if enc.pos >= next_probe {
            enc.skip_span = incompressible_span(enc);
            if enc.skip_span > 0 {
                break;
            }
            next_probe = enc.pos + SKIP_WINDOW;
        }
        enc.encode_next();
        if enc.pos >= next_report {
            if let Some(p) = progress {
                if !p(enc.pos as u64 + enc.pos_offset) {
                    return Err(Error::Cancelled);
                }
            }
            next_report = enc.pos + 16384;
        }
    }
    let rc = std::mem::take(&mut enc.rc);
    Ok((rc.finish(), enc.pos - start))
}

/// Run an encoder over its range and wrap the output in LZMA2 chunks (no end marker).
pub fn encode_block<M: Matches>(
    enc: &mut Encoder<M>,
    mut need_dict_reset: bool,
    mut need_props: bool,
    progress: Option<Progress>,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut need_state_reset = false;
    let props = props_byte(enc.s.lc, enc.s.lp, enc.s.pb);

    fn store(
        out: &mut Vec<u8>,
        data: &[u8],
        stats: &mut Stats,
        need_dict_reset: &mut bool,
        need_props: &mut bool,
        skipped: bool,
    ) {
        for piece in data.chunks(MAX_STORED) {
            out.push(if *need_dict_reset { 1 } else { 2 });
            out.extend_from_slice(&((piece.len() - 1) as u16).to_be_bytes());
            out.extend_from_slice(piece);
            stats.stored_chunks += 1;
            if skipped {
                stats.skipped_bytes += piece.len() as u64;
            }
            if *need_dict_reset {
                *need_dict_reset = false;
                *need_props = true;
            }
        }
    }

    while enc.pos < enc.end {
        let start = enc.pos;
        if need_state_reset {
            enc.model.reset();
        }
        let (packed, size) = encode_chunk(enc, progress)?;
        if size == 0 {
            // stopped right away at incompressible data
        } else if packed.len() >= size {
            // LZMA made it bigger: store raw. The decoder's model is now out of step,
            // so the next LZMA chunk must reset it.
            store(
                &mut out,
                &enc.data[start..start + size],
                &mut enc.stats,
                &mut need_dict_reset,
                &mut need_props,
                false,
            );
            need_state_reset = true;
        } else {
            let reset = if need_dict_reset {
                3
            } else if need_props {
                2
            } else if need_state_reset {
                1
            } else {
                0
            };
            let u = (size - 1) as u32;
            out.push(0x80 | (reset << 5) | (u >> 16) as u8);
            out.extend_from_slice(&((u & 0xFFFF) as u16).to_be_bytes());
            out.extend_from_slice(&((packed.len() - 1) as u16).to_be_bytes());
            if reset >= 2 {
                out.push(props);
            }
            out.extend_from_slice(&packed);
            enc.stats.lzma_chunks += 1;
            need_dict_reset = false;
            need_props = false;
            need_state_reset = false;
        }
        if enc.skip_span > 0 {
            let span = std::mem::take(&mut enc.skip_span);
            enc.clear_plan();
            store(
                &mut out,
                &enc.data[enc.pos..enc.pos + span],
                &mut enc.stats,
                &mut need_dict_reset,
                &mut need_props,
                true,
            );
            need_state_reset = true;
            enc.pos += span;
        }
        if let Some(p) = progress {
            if !p(enc.pos as u64 + enc.pos_offset) {
                return Err(Error::Cancelled);
            }
        }
    }
    Ok(out)
}

// ---- top level: buffers, shared table, parallel slices, auto props -----------------

fn plan_buffers(n: usize, s: &Settings) -> Vec<(usize, usize, usize)> {
    let dict = s.dict_size as usize;
    let overlap = dict / 16 * s.overlap as usize;
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        // later buffers need at least 1 byte of history (literal context, rep0 = 1)
        let lookback = if start == 0 {
            0
        } else {
            start.min(overlap.max(1))
        };
        let end = (start + dict - lookback).min(n);
        out.push((start, end, lookback));
        start = end;
    }
    out
}

fn plan_slices(
    lookback: usize,
    size: usize,
    threads: usize,
    min_slice: usize,
) -> Vec<(usize, usize)> {
    let count = (size / min_slice).clamp(1, threads.max(1));
    let step = size.div_ceil(count);
    (0..count)
        .map(|i| (lookback + i * step, lookback + ((i + 1) * step).min(size)))
        .collect()
}

fn encode_range<M: Matches>(
    buf: &[u8],
    mf: &M,
    start: usize,
    end: usize,
    pos_offset: u64,
    s: &Settings,
    first: bool,
    progress: Option<Progress>,
) -> Result<(Vec<u8>, Stats)> {
    let mut enc = Encoder::new(buf, s, mf, start, end, pos_offset);
    let out = encode_block(&mut enc, first, true, progress)?;
    Ok((out, enc.stats))
}

const PROP_CANDIDATES: [(u32, u32, u32); 6] = [
    (3, 0, 2),
    (4, 0, 0),
    (3, 0, 0),
    (0, 2, 2),
    (2, 0, 1),
    (1, 1, 1),
];
const TUNE_SAMPLE_MAX: usize = 48 << 10;
const TUNE_SAMPLE_MIN: usize = 4 << 10;
const TUNE_MIN_INPUT: usize = 32 << 10;

/// Pick lc/lp/pb by compressing a sample with each candidate (in parallel) and
/// keeping the smallest. The sample is an eighth of the file, 4-48 KiB, taken a
/// quarter of the way in so file headers do not skew it.
pub fn tune_props(data: &[u8], s: &Settings) -> Settings {
    let n = data.len();
    if !s.auto_props || n < TUNE_MIN_INPUT {
        return s.clone();
    }
    let mut sample_size = (n / 8).clamp(TUNE_SAMPLE_MIN, TUNE_SAMPLE_MAX);
    if s.strategy == Strategy::Opt {
        sample_size /= 2;
    }
    let start = n / 4;
    let sample = &data[start..(start + sample_size).min(n)];
    let probe = Settings {
        auto_props: false,
        skip_incompressible: false,
        radix_depth: s.radix_depth.min(16),
        nice_len: s.nice_len.min(32),
        opt_len: s.opt_len.min(128),
        ..s.clone()
    };
    let mf = RadixMatchFinder::build(sample, &probe);
    let best = PROP_CANDIDATES
        .par_iter()
        .map(|&(lc, lp, pb)| {
            let st = Settings {
                lc,
                lp,
                pb,
                ..probe.clone()
            };
            let size = encode_range(sample, &mf, 0, sample.len(), 0, &st, true, None)
                .map(|(o, _)| o.len())
                .unwrap_or(usize::MAX);
            (size, lc, lp, pb)
        })
        .min()
        .unwrap();
    Settings {
        lc: best.1,
        lp: best.2,
        pb: best.3,
        ..s.clone()
    }
}

/// Compress `data` into an LZMA2 stream (with end marker). Returns the stream, the
/// settings actually used (after auto-props) and statistics.
pub fn encode(
    data: &[u8],
    settings: &Settings,
    progress: Option<Progress>,
) -> Result<(Vec<u8>, Settings, Stats)> {
    settings.validate()?;
    let s = tune_props(data, settings);
    let n = data.len();
    let threads = rayon::current_num_threads();
    let mut out = Vec::new();
    let mut stats = Stats::default();
    for (i, (start, end, lb)) in plan_buffers(n, &s).into_iter().enumerate() {
        let buf = &data[start - lb..end];
        let mf = RadixMatchFinder::build(buf, &s);
        let pos_offset = (start - lb) as u64;
        let slices = if n < MIN_POOL_INPUT {
            vec![(lb, buf.len())]
        } else {
            plan_slices(lb, buf.len() - lb, threads, s.min_slice)
        };
        // progress: slices run concurrently, each reports its own absolute position;
        // the sum of their advances plus the bytes of earlier buffers is the total
        let done = AtomicU64::new(start as u64);
        let last: Vec<AtomicU64> = slices
            .iter()
            .map(|&(st, _)| AtomicU64::new(st as u64 + pos_offset))
            .collect();
        let pieces: Vec<Result<(Vec<u8>, Stats)>> = slices
            .par_iter()
            .enumerate()
            .map(|(k, &(ss, se))| {
                let report = |abs: u64| -> bool {
                    let prev = last[k].swap(abs, Relaxed);
                    let advance = abs.saturating_sub(prev);
                    let total = done.fetch_add(advance, Relaxed) + advance;
                    progress.is_none_or(|p| p(total))
                };
                let report_ref: &(dyn Fn(u64) -> bool + Sync) = &report;
                encode_range(
                    buf,
                    &mf,
                    ss,
                    se,
                    pos_offset,
                    &s,
                    i == 0 && k == 0,
                    progress.map(|_| report_ref),
                )
            })
            .collect();
        for piece in pieces {
            let (bytes, st) = piece?;
            out.extend_from_slice(&bytes);
            stats.merge(&st);
        }
    }
    out.push(0x00);
    Ok((out, s, stats))
}

// ---- Decoder --------------------------------------------------------------------------

pub struct Lzma2Decoder {
    pub dec: Decoder,
    need_dict_reset: bool,
    need_props: bool,
    pub props: Option<(u32, u32, u32)>,
}

impl Lzma2Decoder {
    pub fn new(dict_size: u32, size_hint: usize) -> Self {
        Lzma2Decoder {
            dec: Decoder::new(dict_size, size_hint),
            need_dict_reset: true,
            need_props: true,
            props: None,
        }
    }

    /// Decode chunks from buf[pos..]. Returns the position after the end marker.
    pub fn decode(&mut self, buf: &[u8], mut pos: usize, max_output: usize) -> Result<usize> {
        loop {
            let control = *buf
                .get(pos)
                .ok_or_else(|| Error::Corrupt("LZMA2 data ended without an end marker".into()))?;
            pos += 1;
            if control == 0x00 {
                return Ok(pos);
            }
            if control >= 0xE0 || control == 0x01 {
                self.need_props = true;
                self.need_dict_reset = false;
                self.dec.reset_dict();
            } else if self.need_dict_reset {
                return Err(Error::Corrupt(
                    "LZMA2 data does not start with a dictionary reset".into(),
                ));
            }
            if control >= 0x80 {
                let h = buf
                    .get(pos..pos + 4)
                    .ok_or_else(|| Error::Corrupt("truncated LZMA2 chunk header".into()))?;
                let unpacked =
                    (((control & 0x1F) as usize) << 16 | (h[0] as usize) << 8 | h[1] as usize) + 1;
                let packed = ((h[2] as usize) << 8 | h[3] as usize) + 1;
                pos += 4;
                let reset = (control >> 5) & 3;
                if reset >= 2 {
                    let b = *buf
                        .get(pos)
                        .ok_or_else(|| Error::Corrupt("truncated LZMA2 properties".into()))?;
                    pos += 1;
                    let (lc, lp, pb) = parse_props_byte(b)?;
                    self.dec.model = Some(Model::new(lc, lp, pb));
                    self.props = Some((lc, lp, pb));
                    self.need_props = false;
                } else if self.need_props {
                    return Err(Error::Corrupt(
                        "LZMA2 chunk is missing its properties".into(),
                    ));
                } else if reset == 1 {
                    if let Some(m) = self.dec.model.as_mut() {
                        m.reset();
                    }
                }
                if self.dec.out.len() + unpacked > max_output {
                    return Err(Error::Corrupt("output larger than declared".into()));
                }
                let chunk = buf
                    .get(pos..pos + packed)
                    .ok_or_else(|| Error::Corrupt("truncated LZMA2 chunk".into()))?;
                let mut rc = RangeDecoder::new(chunk)?;
                self.dec.decode(&mut rc, unpacked)?;
                if !rc.is_finished() {
                    return Err(Error::Corrupt(
                        "LZMA2 chunk has leftover compressed data".into(),
                    ));
                }
                pos += packed;
            } else if control == 0x01 || control == 0x02 {
                let h = buf
                    .get(pos..pos + 2)
                    .ok_or_else(|| Error::Corrupt("truncated LZMA2 chunk header".into()))?;
                let size = ((h[0] as usize) << 8 | h[1] as usize) + 1;
                pos += 2;
                let piece = buf
                    .get(pos..pos + size)
                    .ok_or_else(|| Error::Corrupt("truncated stored chunk".into()))?;
                if self.dec.out.len() + size > max_output {
                    return Err(Error::Corrupt("output larger than declared".into()));
                }
                self.dec.out.extend_from_slice(piece);
                pos += size;
            } else {
                return Err(Error::Corrupt(format!(
                    "invalid LZMA2 control byte 0x{control:02X}"
                )));
            }
        }
    }
}
