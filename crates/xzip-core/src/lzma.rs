//! LZMA proper: the probability model, the symbol coder, the fast parser and the
//! decoder. LZMA2 chunking lives in lzma2.rs, optimal parsing in optimal.rs.
//!
//! The encoder works on a buffer that may be only part of the file: it codes
//! buffer[start..end] and may reference buffer[..start] as history. `pos_offset`
//! is the absolute position of buffer[0], which the position-dependent contexts
//! (pb, lp) need.

use std::collections::VecDeque;

use crate::error::{Error, Result};
use crate::optimal::OptimalParser;
use crate::rangecoder::{CorruptRange, RangeDecoder, RangeEncoder, PROB_INIT};
use crate::settings::{Settings, Strategy, MATCH_LEN_MAX};

pub const MATCH_LEN_MIN: u32 = 2;
pub const NUM_STATES: usize = 12;
pub const LIT_STATES: u8 = 7;
pub const DIST_SLOTS: usize = 64;
pub const END_POS_MODEL_INDEX: u32 = 14;
pub const FULL_DISTANCES: u32 = 128;
pub const ALIGN_BITS: u32 = 4;

// State machine: 0-6 "last symbol was a literal", 7-11 "was a match of some kind".
pub const NEXT_LITERAL: [u8; 12] = [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 4, 5];
pub const NEXT_MATCH: [u8; 12] = [7, 7, 7, 7, 7, 7, 7, 10, 10, 10, 10, 10];
pub const NEXT_REP: [u8; 12] = [8, 8, 8, 8, 8, 8, 8, 11, 11, 11, 11, 11];
pub const NEXT_SHORT_REP: [u8; 12] = [9, 9, 9, 9, 9, 9, 9, 11, 11, 11, 11, 11];

impl From<CorruptRange> for Error {
    fn from(_: CorruptRange) -> Self {
        Error::Corrupt("range coder data is damaged".into())
    }
}

#[inline]
pub fn get_dist_slot(d: u32) -> u32 {
    if d < 4 {
        d
    } else {
        let n = 31 - d.leading_zeros();
        (n << 1) | ((d >> (n - 1)) & 1)
    }
}

/// Bytes data[a..] and data[b..] have in common, up to `limit` (a + limit and
/// b + limit must be in bounds).
#[inline]
pub fn match_length(data: &[u8], a: usize, b: usize, limit: usize) -> usize {
    let x = &data[a..a + limit];
    let y = &data[b..b + limit];
    // compare 8 bytes at a time, then finish byte-wise
    let mut i = 0;
    while i + 8 <= limit {
        let xa = u64::from_le_bytes(x[i..i + 8].try_into().unwrap());
        let ya = u64::from_le_bytes(y[i..i + 8].try_into().unwrap());
        if xa != ya {
            return i + ((xa ^ ya).trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < limit && x[i] == y[i] {
        i += 1;
    }
    i
}

pub fn props_byte(lc: u32, lp: u32, pb: u32) -> u8 {
    ((pb * 5 + lp) * 9 + lc) as u8
}

pub fn parse_props_byte(b: u8) -> Result<(u32, u32, u32)> {
    let b = b as u32;
    if b >= 9 * 5 * 5 {
        return Err(Error::Corrupt("invalid LZMA properties byte".into()));
    }
    let lc = b % 9;
    let rest = b / 9;
    let lp = rest % 5;
    let pb = rest / 5;
    if lc + lp > 4 {
        return Err(Error::Corrupt(
            "invalid LZMA2 properties (lc + lp > 4)".into(),
        ));
    }
    Ok((lc, lp, pb))
}

pub struct LenModel {
    pub choice: [u16; 2],
    pub low: Vec<u16>,
    pub mid: Vec<u16>,
    pub high: Vec<u16>,
}

impl LenModel {
    fn new() -> Self {
        LenModel {
            choice: [PROB_INIT; 2],
            low: vec![PROB_INIT; 16 << 3],
            mid: vec![PROB_INIT; 16 << 3],
            high: vec![PROB_INIT; 256],
        }
    }
}

/// All adaptive probabilities plus the state LZMA carries between symbols.
pub struct Model {
    pub lc: u32,
    pub lp: u32,
    pub pb: u32,
    pub pb_mask: u64,
    pub lp_mask: u64,
    pub state: u8,
    pub reps: [u32; 4],
    pub is_match: Vec<u16>,
    pub is_rep: [u16; NUM_STATES],
    pub is_rep_g0: [u16; NUM_STATES],
    pub is_rep_g1: [u16; NUM_STATES],
    pub is_rep_g2: [u16; NUM_STATES],
    pub is_rep0_long: Vec<u16>,
    pub literal: Vec<u16>,
    pub dist_slot: Vec<u16>,
    pub dist_special: Vec<u16>,
    pub dist_align: [u16; 16],
    pub match_len: LenModel,
    pub rep_len: LenModel,
}

impl Model {
    pub fn new(lc: u32, lp: u32, pb: u32) -> Self {
        Model {
            lc,
            lp,
            pb,
            pb_mask: (1u64 << pb) - 1,
            lp_mask: (1u64 << lp) - 1,
            state: 0,
            reps: [0; 4],
            is_match: vec![PROB_INIT; NUM_STATES << 4],
            is_rep: [PROB_INIT; NUM_STATES],
            is_rep_g0: [PROB_INIT; NUM_STATES],
            is_rep_g1: [PROB_INIT; NUM_STATES],
            is_rep_g2: [PROB_INIT; NUM_STATES],
            is_rep0_long: vec![PROB_INIT; NUM_STATES << 4],
            literal: vec![PROB_INIT; 0x300 << (lc + lp)],
            dist_slot: vec![PROB_INIT; 4 << 6],
            dist_special: vec![PROB_INIT; (1 + FULL_DISTANCES - END_POS_MODEL_INDEX) as usize],
            dist_align: [PROB_INIT; 16],
            match_len: LenModel::new(),
            rep_len: LenModel::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Model::new(self.lc, self.lp, self.pb);
    }

    #[inline]
    pub fn literal_base(&self, pos: u64, prev: u8) -> usize {
        let lit_state =
            ((pos & self.lp_mask) << self.lc) as usize + ((prev as usize) >> (8 - self.lc));
        0x300 * lit_state
    }
}

/// Where matches come from. The radix finder implements this; the encoder does
/// not care how the table was built.
pub trait Matches: Sync {
    /// Longest match at `pos` as (length, distance), (0, 0) if none.
    fn at(&self, pos: usize) -> (u32, u32);
    /// Up to a few matches for the optimal parser, longest first.
    fn candidates(&self, pos: usize, out: &mut Vec<(u32, u32)>);
    /// Cheap recorded match length, for the incompressible-data probe.
    fn probe(&self, pos: usize) -> u32;
}

#[derive(Clone, Copy, Debug)]
pub enum Symbol {
    Literal,
    Match { len: u32, dist: u32 },
    Rep { len: u32, idx: u32 },
    ShortRep,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub literals: u64,
    pub matches: u64,
    pub reps: u64,
    pub short_reps: u64,
    pub lzma_chunks: u64,
    pub stored_chunks: u64,
    pub skipped_bytes: u64,
    /// Pre-filter actually applied (set by the .xz layer).
    pub filter: Option<crate::filters::Filter>,
}

impl Stats {
    pub fn merge(&mut self, o: &Stats) {
        self.literals += o.literals;
        self.matches += o.matches;
        self.reps += o.reps;
        self.short_reps += o.short_reps;
        self.lzma_chunks += o.lzma_chunks;
        self.stored_chunks += o.stored_chunks;
        self.skipped_bytes += o.skipped_bytes;
    }
}

pub struct Encoder<'a, M: Matches> {
    pub data: &'a [u8],
    pub s: &'a Settings,
    pub matches: &'a M,
    pub model: Model,
    pub pos: usize,
    pub end: usize,
    pub pos_offset: u64,
    pub rc: RangeEncoder,
    pub skip_span: usize,
    pub stats: Stats,
    queue: VecDeque<Symbol>,
    parser: Option<OptimalParser>,
}

impl<'a, M: Matches> Encoder<'a, M> {
    pub fn new(
        data: &'a [u8],
        s: &'a Settings,
        matches: &'a M,
        start: usize,
        end: usize,
        pos_offset: u64,
    ) -> Self {
        let parser = if s.strategy == Strategy::Opt {
            Some(OptimalParser::new(s))
        } else {
            None
        };
        Encoder {
            data,
            s,
            matches,
            model: Model::new(s.lc, s.lp, s.pb),
            pos: start,
            end,
            pos_offset,
            rc: RangeEncoder::new(),
            skip_span: 0,
            stats: Stats::default(),
            queue: VecDeque::new(),
            parser,
        }
    }

    #[inline]
    fn pos_state(&self) -> usize {
        ((self.pos as u64 + self.pos_offset) & self.model.pb_mask) as usize
    }

    pub fn encode_next(&mut self) {
        let sym = if let Some(parser) = self.parser.as_mut() {
            if self.queue.is_empty() {
                let (data, matches, model) = (self.data, self.matches, &self.model);
                parser.parse(
                    data,
                    matches,
                    model,
                    self.pos,
                    self.end,
                    self.pos_offset,
                    &mut self.queue,
                );
            }
            self.queue
                .pop_front()
                .expect("parser returns at least one symbol")
        } else {
            self.choose()
        };
        match sym {
            Symbol::Literal => {
                self.stats.literals += 1;
                self.literal();
                self.pos += 1;
            }
            Symbol::Match { len, dist } => {
                self.stats.matches += 1;
                self.match_(len, dist);
                self.pos += len as usize;
            }
            Symbol::Rep { len, idx } => {
                self.stats.reps += 1;
                self.rep(len, idx);
                self.pos += len as usize;
            }
            Symbol::ShortRep => {
                self.stats.short_reps += 1;
                self.short_rep();
                self.pos += 1;
            }
        }
    }

    /// Drop anything the optimal parser planned (used when a region is stored raw).
    pub fn clear_plan(&mut self) {
        self.queue.clear();
    }

    fn best_rep(&self, pos: usize, avail: usize) -> (u32, u32) {
        let (mut best_len, mut best_idx) = (0u32, 0u32);
        if avail < 2 {
            return (0, 0);
        }
        let data = self.data;
        for (i, &r) in self.model.reps.iter().enumerate() {
            let back = r as usize + 1;
            if back > pos || data[pos - back] != data[pos] || data[pos - back + 1] != data[pos + 1]
            {
                continue;
            }
            let len = match_length(data, pos - back, pos, avail) as u32;
            if len > best_len {
                best_len = len;
                best_idx = i as u32;
            }
        }
        (best_len, best_idx)
    }

    fn literal_or_short_rep(&self, pos: usize) -> Symbol {
        let back = self.model.reps[0] as usize + 1;
        if self.s.short_rep && back <= pos && self.data[pos - back] == self.data[pos] {
            Symbol::ShortRep
        } else {
            Symbol::Literal
        }
    }

    /// The fast parser: greedy with one lazy step (also rep-aware, as in xz).
    fn choose(&self) -> Symbol {
        let (data, pos, s) = (self.data, self.pos, self.s);
        let avail = (self.end - pos).min(MATCH_LEN_MAX as usize);
        let (mut main_len, main_dist) = self.matches.at(pos);
        if main_len as usize > avail {
            main_len = avail as u32;
        }
        let (rep_len, rep_idx) = self.best_rep(pos, avail);

        if rep_len >= s.nice_len {
            return Symbol::Rep {
                len: rep_len,
                idx: rep_idx,
            };
        }
        if main_len >= s.nice_len {
            return Symbol::Match {
                len: main_len,
                dist: main_dist,
            };
        }
        if main_len == 3 && main_dist > s.len3_max_dist {
            main_len = 0;
        }
        if rep_len >= 2
            && (rep_len + s.rep_bias >= main_len
                || (rep_len + s.rep_bias + 1 >= main_len && main_dist > 1 << 9)
                || (rep_len + s.rep_bias + 2 >= main_len && main_dist > 1 << 15))
        {
            return Symbol::Rep {
                len: rep_len,
                idx: rep_idx,
            };
        }
        if main_len < 2 {
            return self.literal_or_short_rep(pos);
        }
        if s.lazy && pos + 1 < self.end {
            let (next_len, next_dist) = self.matches.at(pos + 1);
            if next_len >= 2
                && ((next_len >= main_len && next_dist < main_dist)
                    || (next_len == main_len + 1 && (next_dist >> 7) <= main_dist)
                    || next_len > main_len + 1
                    || (next_len + 1 >= main_len && main_len >= 3 && (main_dist >> 7) > next_dist))
            {
                return self.literal_or_short_rep(pos);
            }
            // A rep at the next position nearly as long as this match is the better
            // deal (no distance to code); a literal does not change the reps.
            let nxt = pos + 1;
            let limit = (main_len.saturating_sub(1)).max(MATCH_LEN_MIN) as usize;
            let avail1 = (self.end - nxt).min(limit);
            if avail1 >= limit {
                for &r in &self.model.reps {
                    let back = r as usize + 1;
                    if back <= nxt
                        && data[nxt - back] == data[nxt]
                        && match_length(data, nxt - back, nxt, avail1) >= limit
                    {
                        return self.literal_or_short_rep(pos);
                    }
                }
            }
        }
        Symbol::Match {
            len: main_len,
            dist: main_dist,
        }
    }

    fn literal(&mut self) {
        let m = &mut self.model;
        let pos = self.pos;
        let abs = pos as u64 + self.pos_offset;
        let ps = (abs & m.pb_mask) as usize;
        self.rc
            .encode_bit(&mut m.is_match[((m.state as usize) << 4) | ps], 0);
        let prev = if pos > 0 { self.data[pos - 1] } else { 0 };
        let base = m.literal_base(abs, prev);
        let byte = self.data[pos] as u32;
        let mut symbol = 1u32;
        if m.state >= LIT_STATES {
            // Right after a match the byte that would have continued it is a strong
            // hint; while our bits agree with it, use a separate probability set.
            let match_byte = self.data[pos - m.reps[0] as usize - 1] as u32;
            while symbol < 0x100 {
                let shift = 8 - (32 - symbol.leading_zeros());
                let match_bit = (match_byte >> shift) & 1;
                let bit = (byte >> shift) & 1;
                self.rc.encode_bit(
                    &mut m.literal[base + (((1 + match_bit) << 8) + symbol) as usize],
                    bit,
                );
                symbol = (symbol << 1) | bit;
                if bit != match_bit {
                    break;
                }
            }
        }
        while symbol < 0x100 {
            let shift = 8 - (32 - symbol.leading_zeros());
            let bit = (byte >> shift) & 1;
            self.rc
                .encode_bit(&mut m.literal[base + symbol as usize], bit);
            symbol = (symbol << 1) | bit;
        }
        m.state = NEXT_LITERAL[m.state as usize];
    }

    fn encode_len(rc: &mut RangeEncoder, lm: &mut LenModel, len: u32, ps: usize) {
        let n = len - MATCH_LEN_MIN;
        if n < 8 {
            rc.encode_bit(&mut lm.choice[0], 0);
            rc.encode_tree(&mut lm.low[ps << 3..], 3, n);
        } else if n < 16 {
            rc.encode_bit(&mut lm.choice[0], 1);
            rc.encode_bit(&mut lm.choice[1], 0);
            rc.encode_tree(&mut lm.mid[ps << 3..], 3, n - 8);
        } else {
            rc.encode_bit(&mut lm.choice[0], 1);
            rc.encode_bit(&mut lm.choice[1], 1);
            rc.encode_tree(&mut lm.high, 8, n - 16);
        }
    }

    fn match_(&mut self, len: u32, dist: u32) {
        let ps = self.pos_state();
        let m = &mut self.model;
        let st = m.state as usize;
        self.rc.encode_bit(&mut m.is_match[(st << 4) | ps], 1);
        self.rc.encode_bit(&mut m.is_rep[st], 0);
        Self::encode_len(&mut self.rc, &mut m.match_len, len, ps);
        let d = dist - 1;
        let slot = get_dist_slot(d);
        let len_state = (len - MATCH_LEN_MIN).min(3) as usize;
        self.rc
            .encode_tree(&mut m.dist_slot[len_state << 6..], 6, slot);
        if slot >= 4 {
            let footer = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << footer;
            let reduced = d - base;
            if slot < END_POS_MODEL_INDEX {
                self.rc.encode_reverse_tree(
                    &mut m.dist_special[(base - slot) as usize..],
                    footer,
                    reduced,
                );
            } else {
                self.rc
                    .encode_direct_bits(reduced >> ALIGN_BITS, footer - ALIGN_BITS);
                self.rc
                    .encode_reverse_tree(&mut m.dist_align, ALIGN_BITS, reduced & 0xF);
            }
        }
        m.reps = [d, m.reps[0], m.reps[1], m.reps[2]];
        m.state = NEXT_MATCH[st];
    }

    fn rep(&mut self, len: u32, idx: u32) {
        let ps = self.pos_state();
        let m = &mut self.model;
        let st = m.state as usize;
        self.rc.encode_bit(&mut m.is_match[(st << 4) | ps], 1);
        self.rc.encode_bit(&mut m.is_rep[st], 1);
        if idx == 0 {
            self.rc.encode_bit(&mut m.is_rep_g0[st], 0);
            self.rc.encode_bit(&mut m.is_rep0_long[(st << 4) | ps], 1);
        } else {
            self.rc.encode_bit(&mut m.is_rep_g0[st], 1);
            if idx == 1 {
                self.rc.encode_bit(&mut m.is_rep_g1[st], 0);
            } else {
                self.rc.encode_bit(&mut m.is_rep_g1[st], 1);
                self.rc.encode_bit(&mut m.is_rep_g2[st], idx - 2);
            }
            let d = m.reps[idx as usize];
            for i in (1..=idx as usize).rev() {
                m.reps[i] = m.reps[i - 1];
            }
            m.reps[0] = d;
        }
        Self::encode_len(&mut self.rc, &mut m.rep_len, len, ps);
        m.state = NEXT_REP[st];
    }

    fn short_rep(&mut self) {
        let ps = self.pos_state();
        let m = &mut self.model;
        let st = m.state as usize;
        self.rc.encode_bit(&mut m.is_match[(st << 4) | ps], 1);
        self.rc.encode_bit(&mut m.is_rep[st], 1);
        self.rc.encode_bit(&mut m.is_rep_g0[st], 0);
        self.rc.encode_bit(&mut m.is_rep0_long[(st << 4) | ps], 0);
        m.state = NEXT_SHORT_REP[st];
    }
}

// ---- Decoder ------------------------------------------------------------------------

pub struct Decoder {
    pub out: Vec<u8>,
    dict_size: usize,
    dict_start: usize,
    pub model: Option<Model>,
}

impl Decoder {
    pub fn new(dict_size: u32, size_hint: usize) -> Self {
        Decoder {
            out: Vec::with_capacity(size_hint),
            dict_size: dict_size as usize,
            dict_start: 0,
            model: None,
        }
    }

    pub fn reset_dict(&mut self) {
        self.dict_start = self.out.len();
    }

    pub fn decode(&mut self, rc: &mut RangeDecoder, count: usize) -> Result<()> {
        let end = self.out.len() + count;
        let m = self
            .model
            .as_mut()
            .ok_or_else(|| Error::Corrupt("LZMA chunk without properties".into()))?;
        while self.out.len() < end {
            let pos = (self.out.len() - self.dict_start) as u64;
            let ps = (pos & m.pb_mask) as usize;
            let st = m.state as usize;
            if rc.decode_bit(&mut m.is_match[(st << 4) | ps])? == 0 {
                Self::literal(&mut self.out, m, rc, pos)?;
                continue;
            }
            let len;
            if rc.decode_bit(&mut m.is_rep[st])? == 0 {
                len = Self::decode_len(rc, &mut m.match_len, ps)?;
                let d = Self::decode_dist(rc, m, len)?;
                m.reps = [d, m.reps[0], m.reps[1], m.reps[2]];
                m.state = NEXT_MATCH[st];
            } else {
                if rc.decode_bit(&mut m.is_rep_g0[st])? == 0 {
                    if rc.decode_bit(&mut m.is_rep0_long[(st << 4) | ps])? == 0 {
                        m.state = NEXT_SHORT_REP[st];
                        Self::copy(
                            &mut self.out,
                            self.dict_start,
                            self.dict_size,
                            m.reps[0] as usize + 1,
                            1,
                            end,
                        )?;
                        continue;
                    }
                } else {
                    let idx = if rc.decode_bit(&mut m.is_rep_g1[st])? == 0 {
                        1
                    } else {
                        2 + rc.decode_bit(&mut m.is_rep_g2[st])? as usize
                    };
                    let d = m.reps[idx];
                    for i in (1..=idx).rev() {
                        m.reps[i] = m.reps[i - 1];
                    }
                    m.reps[0] = d;
                }
                len = Self::decode_len(rc, &mut m.rep_len, ps)?;
                m.state = NEXT_REP[st];
            }
            Self::copy(
                &mut self.out,
                self.dict_start,
                self.dict_size,
                m.reps[0] as usize + 1,
                len as usize,
                end,
            )?;
        }
        Ok(())
    }

    fn copy(
        out: &mut Vec<u8>,
        dict_start: usize,
        dict_size: usize,
        dist: usize,
        len: usize,
        end: usize,
    ) -> Result<()> {
        let pos = out.len() - dict_start;
        if dist > pos || dist > dict_size {
            return Err(Error::Corrupt(
                "match distance points before the start of the data".into(),
            ));
        }
        if out.len() + len > end {
            return Err(Error::Corrupt(
                "match runs past the end of the chunk".into(),
            ));
        }
        let start = out.len() - dist;
        if dist >= len {
            out.extend_from_within(start..start + len);
        } else {
            for i in 0..len {
                let b = out[start + i];
                out.push(b);
            }
        }
        Ok(())
    }

    fn literal(out: &mut Vec<u8>, m: &mut Model, rc: &mut RangeDecoder, pos: u64) -> Result<()> {
        let prev = if pos > 0 { out[out.len() - 1] } else { 0 };
        let base = m.literal_base(pos, prev);
        let mut symbol = 1u32;
        if m.state >= LIT_STATES {
            let back = m.reps[0] as usize + 1;
            if back > out.len() {
                return Err(Error::Corrupt(
                    "literal context points before the data".into(),
                ));
            }
            let match_byte = out[out.len() - back] as u32;
            while symbol < 0x100 {
                let shift = 8 - (32 - symbol.leading_zeros());
                let match_bit = (match_byte >> shift) & 1;
                let bit = rc.decode_bit(
                    &mut m.literal[base + (((1 + match_bit) << 8) + symbol) as usize],
                )?;
                symbol = (symbol << 1) | bit;
                if bit != match_bit {
                    break;
                }
            }
        }
        while symbol < 0x100 {
            symbol = (symbol << 1) | rc.decode_bit(&mut m.literal[base + symbol as usize])?;
        }
        out.push((symbol & 0xFF) as u8);
        m.state = NEXT_LITERAL[m.state as usize];
        Ok(())
    }

    fn decode_len(rc: &mut RangeDecoder, lm: &mut LenModel, ps: usize) -> Result<u32> {
        if rc.decode_bit(&mut lm.choice[0])? == 0 {
            return Ok(MATCH_LEN_MIN + rc.decode_tree(&mut lm.low[ps << 3..], 3)?);
        }
        if rc.decode_bit(&mut lm.choice[1])? == 0 {
            return Ok(MATCH_LEN_MIN + 8 + rc.decode_tree(&mut lm.mid[ps << 3..], 3)?);
        }
        Ok(MATCH_LEN_MIN + 16 + rc.decode_tree(&mut lm.high, 8)?)
    }

    fn decode_dist(rc: &mut RangeDecoder, m: &mut Model, len: u32) -> Result<u32> {
        let len_state = (len - MATCH_LEN_MIN).min(3) as usize;
        let slot = rc.decode_tree(&mut m.dist_slot[len_state << 6..], 6)?;
        if slot < 4 {
            return Ok(slot);
        }
        let footer = (slot >> 1) - 1;
        let base = (2 | (slot & 1)) << footer;
        if slot < END_POS_MODEL_INDEX {
            return Ok(base
                + rc.decode_reverse_tree(&mut m.dist_special[(base - slot) as usize..], footer)?);
        }
        let direct = rc.decode_direct_bits(footer - ALIGN_BITS)?;
        Ok(
            base + (direct << ALIGN_BITS)
                + rc.decode_reverse_tree(&mut m.dist_align, ALIGN_BITS)?,
        )
    }
}
