//! Optimal parsing: plan a window ahead, price every literal/match/rep alternative in
//! output bits with the current probabilities, and take the cheapest path. A
//! shortest-path search over positions, like xz's normal mode.
//!
//! Prices are estimates: probabilities drift as symbols are coded, so length and
//! distance tables are rebuilt every `price_refresh` symbols. Any parse is a valid
//! parse, only its cost is approximate.

use std::collections::VecDeque;

use crate::lzma::{
    get_dist_slot, match_length, LenModel, Matches, Model, Symbol, ALIGN_BITS, DIST_SLOTS,
    END_POS_MODEL_INDEX, FULL_DISTANCES, LIT_STATES, NEXT_LITERAL, NEXT_MATCH, NEXT_REP,
    NEXT_SHORT_REP,
};
use crate::rangecoder::{price, price0, price1};
use crate::settings::{Settings, MATCH_LEN_MAX};

const INF: u32 = u32::MAX / 4;
const LONG_MATCH_WEIGHT: u32 = 16;

#[derive(Clone, Copy)]
struct Back {
    prev: u32,
    sym: Symbol,
}

pub struct OptimalParser {
    opt_len: usize,
    nice_len: u32,
    refresh_every: u32,
    since_refresh: u32,
    match_len_prices: Vec<Vec<u32>>, // [pos_state][len]
    rep_len_prices: Vec<Vec<u32>>,
    slot_prices: [Vec<u32>; 4],
    align_prices: [u32; 16],
    full_dist_prices: Vec<[u32; 4]>, // [d][len_state] for d < 128
    // per-window node arrays, kept to avoid reallocation
    price: Vec<u32>,
    state: Vec<u8>,
    reps: Vec<[u32; 4]>,
    back: Vec<Back>,
    cands: Vec<(u32, u32)>,
}

fn all_tree_prices(probs: &[u16], num_bits: u32) -> Vec<u32> {
    // walk the tree level by level so every node is priced once
    let mut level = vec![0u32];
    let mut node = 1usize;
    for _ in 0..num_bits {
        let mut next = Vec::with_capacity(level.len() * 2);
        for &p in &level {
            let prob = probs[node];
            next.push(p + price0(prob));
            next.push(p + price1(prob));
            node += 1;
        }
        level = next;
    }
    level
}

fn reverse_tree_price(probs: &[u16], num_bits: u32, mut value: u32) -> u32 {
    let (mut m, mut total) = (1usize, 0u32);
    for _ in 0..num_bits {
        let bit = value & 1;
        value >>= 1;
        total += price(probs[m], bit);
        m = (m << 1) | bit as usize;
    }
    total
}

fn length_prices(lm: &LenModel, ps: usize) -> Vec<u32> {
    let (c00, c01) = (price0(lm.choice[0]), price1(lm.choice[0]));
    let (c10, c11) = (price0(lm.choice[1]), price1(lm.choice[1]));
    let mut out = vec![INF, INF];
    out.extend(
        all_tree_prices(&lm.low[ps << 3..], 3)
            .into_iter()
            .map(|p| c00 + p),
    );
    out.extend(
        all_tree_prices(&lm.mid[ps << 3..], 3)
            .into_iter()
            .map(|p| c01 + c10 + p),
    );
    out.extend(
        all_tree_prices(&lm.high, 8)
            .into_iter()
            .map(|p| c01 + c11 + p),
    );
    out
}

impl OptimalParser {
    pub fn new(s: &Settings) -> Self {
        let cap = s.opt_len as usize + MATCH_LEN_MAX as usize + 2;
        OptimalParser {
            opt_len: s.opt_len as usize,
            nice_len: s.nice_len,
            refresh_every: s.price_refresh,
            since_refresh: s.price_refresh,
            match_len_prices: Vec::new(),
            rep_len_prices: Vec::new(),
            slot_prices: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            align_prices: [0; 16],
            full_dist_prices: Vec::new(),
            price: vec![INF; cap],
            state: vec![0; cap],
            reps: vec![[0; 4]; cap],
            back: vec![
                Back {
                    prev: 0,
                    sym: Symbol::Literal
                };
                cap
            ],
            cands: Vec::with_capacity(4),
        }
    }

    fn refresh(&mut self, m: &Model) {
        let pb_states = 1usize << m.pb;
        self.match_len_prices = (0..pb_states)
            .map(|ps| length_prices(&m.match_len, ps))
            .collect();
        self.rep_len_prices = (0..pb_states)
            .map(|ps| length_prices(&m.rep_len, ps))
            .collect();
        for ls in 0..4 {
            self.slot_prices[ls] = all_tree_prices(&m.dist_slot[ls << 6..], 6);
        }
        for (a, slot) in self.align_prices.iter_mut().enumerate() {
            *slot = reverse_tree_price(&m.dist_align, ALIGN_BITS, a as u32);
        }
        let mut special = vec![0u32; 4];
        for slot in 4..END_POS_MODEL_INDEX {
            let footer = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << footer;
            for i in 0..(1u32 << footer) {
                special.push(reverse_tree_price(
                    &m.dist_special[(base - slot) as usize..],
                    footer,
                    i,
                ));
            }
        }
        self.full_dist_prices = (0..FULL_DISTANCES as usize)
            .map(|d| {
                let slot = get_dist_slot(d as u32) as usize;
                [0, 1, 2, 3].map(|ls| self.slot_prices[ls][slot] + special[d])
            })
            .collect();
        self.since_refresh = 0;
    }

    #[inline]
    fn dist_prices(&self, d: u32) -> [u32; 4] {
        if d < FULL_DISTANCES {
            return self.full_dist_prices[d as usize];
        }
        let slot = get_dist_slot(d) as usize;
        let footer = (slot as u32 >> 1) - 1;
        let extra = ((footer - ALIGN_BITS) << 4) + self.align_prices[(d & 0xF) as usize];
        [0, 1, 2, 3].map(|ls| self.slot_prices[ls][slot] + extra)
    }

    fn literal_price(
        data: &[u8],
        m: &Model,
        p: usize,
        pos_offset: u64,
        state: u8,
        rep0: u32,
    ) -> u32 {
        let prev = if p > 0 { data[p - 1] } else { 0 };
        let base = m.literal_base(p as u64 + pos_offset, prev);
        let probs = &m.literal;
        let byte = data[p] as u32;
        let mut symbol = 1u32;
        let mut total = 0u32;
        if state >= LIT_STATES {
            let match_byte = data[p - rep0 as usize - 1] as u32;
            while symbol < 0x100 {
                let shift = 8 - (32 - symbol.leading_zeros());
                let match_bit = (match_byte >> shift) & 1;
                let bit = (byte >> shift) & 1;
                total += price(
                    probs[base + (((1 + match_bit) << 8) + symbol) as usize],
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
            total += price(probs[base + symbol as usize], bit);
            symbol = (symbol << 1) | bit;
        }
        total
    }

    /// Plan the cheapest symbols for data[start..] and push them onto `out`.
    pub fn parse<M: Matches>(
        &mut self,
        data: &[u8],
        matches: &M,
        m: &Model,
        start: usize,
        end: usize,
        pos_offset: u64,
        out: &mut VecDeque<Symbol>,
    ) {
        if self.since_refresh >= self.refresh_every {
            self.refresh(m);
        }
        let opt_len = self.opt_len;
        let nice_len = self.nice_len;
        let size = (end - start).min(opt_len + MATCH_LEN_MAX as usize) + 1;
        for v in &mut self.price[..size] {
            *v = INF;
        }
        self.price[0] = 0;
        self.state[0] = m.state;
        self.reps[0] = m.reps;
        let mut last = 0usize;
        let mut cut = 0usize;
        let mut i = 0usize;
        while i < opt_len && start + i < end && i <= last {
            let base = self.price[i];
            if base == INF {
                i += 1;
                continue;
            }
            let p = start + i;
            let ps = ((p as u64 + pos_offset) & m.pb_mask) as usize;
            let st = self.state[i];
            let r = self.reps[i];
            let avail = (end - p).min(MATCH_LEN_MAX as usize);
            let idx = ((st as usize) << 4) | ps;
            let pm = m.is_match[idx];

            // literal
            let c = base + price0(pm) + Self::literal_price(data, m, p, pos_offset, st, r[0]);
            let j = i + 1;
            if c < self.price[j] {
                self.price[j] = c;
                self.state[j] = NEXT_LITERAL[st as usize];
                self.reps[j] = r;
                self.back[j] = Back {
                    prev: i as u32,
                    sym: Symbol::Literal,
                };
            }
            last = last.max(j);
            let m1 = base + price1(pm);
            let pr = m.is_rep[st as usize];
            let rp = m1 + price1(pr);

            // short rep
            let back0 = r[0] as usize + 1;
            if back0 <= p && data[p - back0] == data[p] {
                let c = rp + price0(m.is_rep_g0[st as usize]) + price0(m.is_rep0_long[idx]);
                if c < self.price[j] {
                    self.price[j] = c;
                    self.state[j] = NEXT_SHORT_REP[st as usize];
                    self.reps[j] = r;
                    self.back[j] = Back {
                        prev: i as u32,
                        sym: Symbol::ShortRep,
                    };
                }
            }
            if avail < 2 {
                i += 1;
                continue;
            }

            // reps
            let ns = NEXT_REP[st as usize];
            for k in 0..4usize {
                let back = r[k] as usize + 1;
                if back > p || data[p - back] != data[p] || data[p - back + 1] != data[p + 1] {
                    continue;
                }
                let len = match_length(data, p - back, p, avail);
                let (c0, nr) = match k {
                    0 => (
                        rp + price0(m.is_rep_g0[st as usize]) + price1(m.is_rep0_long[idx]),
                        r,
                    ),
                    1 => (
                        rp + price1(m.is_rep_g0[st as usize]) + price0(m.is_rep_g1[st as usize]),
                        [r[1], r[0], r[2], r[3]],
                    ),
                    2 => (
                        rp + price1(m.is_rep_g0[st as usize])
                            + price1(m.is_rep_g1[st as usize])
                            + price0(m.is_rep_g2[st as usize]),
                        [r[2], r[0], r[1], r[3]],
                    ),
                    _ => (
                        rp + price1(m.is_rep_g0[st as usize])
                            + price1(m.is_rep_g1[st as usize])
                            + price1(m.is_rep_g2[st as usize]),
                        [r[3], r[0], r[1], r[2]],
                    ),
                };
                let rlp = &self.rep_len_prices[ps];
                for l in 2..=len {
                    let c = c0 + rlp[l];
                    let j = i + l;
                    if c < self.price[j] {
                        self.price[j] = c;
                        self.state[j] = ns;
                        self.reps[j] = nr;
                        self.back[j] = Back {
                            prev: i as u32,
                            sym: Symbol::Rep {
                                len: l as u32,
                                idx: k as u32,
                            },
                        };
                    }
                }
                last = last.max(i + len);
                if len as u32 >= nice_len {
                    cut = i + len;
                    break;
                }
            }
            if cut != 0 {
                break;
            }

            // new matches: longest first, then shorter/nearer ones
            let ns = NEXT_MATCH[st as usize];
            let c0 = m1 + price0(pr);
            self.cands.clear();
            matches.candidates(p, &mut self.cands);
            let cands = std::mem::take(&mut self.cands);
            for &(len, dist) in &cands {
                let d = dist - 1;
                if r.contains(&d) {
                    continue;
                }
                let len = (len as usize).min(avail);
                if len < 2 {
                    continue;
                }
                let nr = [d, r[0], r[1], r[2]];
                let dp = self.dist_prices(d);
                let mlp = &self.match_len_prices[ps];
                for l in 2..=len {
                    let c = c0 + mlp[l] + dp[(l - 2).min(3)];
                    let j = i + l;
                    if c < self.price[j] {
                        self.price[j] = c;
                        self.state[j] = ns;
                        self.reps[j] = nr;
                        self.back[j] = Back {
                            prev: i as u32,
                            sym: Symbol::Match {
                                len: l as u32,
                                dist,
                            },
                        };
                    }
                }
                last = last.max(i + len);
                if len as u32 >= nice_len {
                    cut = i + len;
                    break;
                }
            }
            self.cands = cands;
            if cut != 0 {
                break;
            }
            i += 1;
        }

        // read the cheapest path back
        let terminal = if cut != 0 { cut } else { last };
        let first = out.len();
        let mut j = terminal;
        while j > 0 {
            let b = self.back[j];
            out.push_back(b.sym);
            j = b.prev as usize;
        }
        out.make_contiguous()[first..].reverse();
        let planned = (out.len() - first) as u32;
        self.since_refresh += planned + if cut != 0 { LONG_MATCH_WEIGHT } else { 0 };
    }
}

#[allow(dead_code)]
const _: () = assert!(DIST_SLOTS == 64);
