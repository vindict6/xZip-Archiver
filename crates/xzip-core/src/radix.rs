//! Radix match finder. All positions are bucketed by their first two bytes, then
//! each bucket is split by the following bytes, level by level, down to
//! `radix_depth`. Positions that share d bytes end up in one group in file order,
//! so each position's predecessor in its group is its nearest earlier occurrence
//! with that prefix. For every position we keep the longest match found that way,
//! the two shorter-but-nearer matches it displaced on the way down, and the nearest
//! 2-byte match. The encoder extends lengths beyond the depth when it uses a match.
//!
//! Buckets are independent, so they are sorted in parallel. Tables are atomics so
//! threads can fill disjoint positions without locking; the stores are relaxed
//! because nothing reads a position before the build has finished.

use std::sync::atomic::{AtomicU16, AtomicU32, Ordering::Relaxed};

use rayon::prelude::*;

use crate::lzma::{match_length, Matches};
use crate::settings::{Settings, MATCH_LEN_MAX};

const MAX_STEP: u32 = 64;

pub struct RadixMatchFinder<'a> {
    data: &'a [u8],
    n: usize,
    max_depth: usize,
    step: u32,
    lengths: Vec<AtomicU16>,
    dists: Vec<AtomicU32>,
    alt_len: [Vec<AtomicU16>; 2],
    alt_dist: [Vec<AtomicU32>; 2],
    near: Vec<AtomicU32>,
}

fn atomic_u16s(n: usize) -> Vec<AtomicU16> {
    (0..n).map(|_| AtomicU16::new(0)).collect()
}

fn atomic_u32s(n: usize) -> Vec<AtomicU32> {
    (0..n).map(|_| AtomicU32::new(0)).collect()
}

impl<'a> RadixMatchFinder<'a> {
    pub fn build(data: &'a [u8], s: &Settings) -> Self {
        let n = data.len();
        let mf = RadixMatchFinder {
            data,
            n,
            max_depth: s.radix_depth.min(MATCH_LEN_MAX) as usize,
            step: s.radix_step,
            lengths: atomic_u16s(n),
            dists: atomic_u32s(n),
            alt_len: [atomic_u16s(n), atomic_u16s(n)],
            alt_dist: [atomic_u32s(n), atomic_u32s(n)],
            near: atomic_u32s(n),
        };
        if n < 2 {
            return mf;
        }
        // Level 1: counting sort by the first two bytes
        let mut counts = vec![0u32; 65537];
        for w in data.windows(2) {
            counts[((w[0] as usize) << 8 | w[1] as usize) + 1] += 1;
        }
        for k in 0..65536 {
            counts[k + 1] += counts[k];
        }
        let mut order = vec![0u32; n - 1];
        let mut fill = counts.clone();
        for (p, w) in data.windows(2).enumerate() {
            let k = (w[0] as usize) << 8 | w[1] as usize;
            order[fill[k] as usize] = p as u32;
            fill[k] += 1;
        }
        // Cut `order` into one slice per bucket and sort each bucket on its own thread
        let mut slices: Vec<&mut [u32]> = Vec::with_capacity(4096);
        let mut rest: &mut [u32] = &mut order;
        for k in 0..65536 {
            let len = (counts[k + 1] - counts[k]) as usize;
            let (head, tail) = rest.split_at_mut(len);
            rest = tail;
            if len >= 2 {
                slices.push(head);
            }
        }
        slices
            .into_par_iter()
            .for_each(|bucket| mf.sort_bucket(bucket));
        mf
    }

    fn sort_bucket(&self, bucket: &mut [u32]) {
        let data = self.data;
        let n = self.n;
        let max_depth = self.max_depth;
        // level-2 records: nearest earlier occurrence of the same two bytes
        for w in bucket.windows(2) {
            let (prev, p) = (w[0] as usize, w[1] as usize);
            self.lengths[p].store(2, Relaxed);
            self.dists[p].store((p - prev) as u32, Relaxed);
            self.near[p].store((p - prev) as u32, Relaxed);
        }
        if max_depth <= 2 {
            return;
        }
        let mut stack: Vec<(usize, usize, usize, u32)> = vec![(0, bucket.len(), 2, self.step)];
        let mut scratch: Vec<u32> = Vec::new();
        while let Some((start, end, depth, step)) = stack.pop() {
            let nd = (depth + step as usize).min(max_depth);
            let group = &mut bucket[start..end];
            // keep the positions that still have nd bytes ahead, sorted by the next bytes
            scratch.clear();
            scratch.extend(group.iter().copied().filter(|&p| p as usize + nd <= n));
            let eligible = scratch.len();
            if eligible < 2 {
                continue;
            }
            if nd - depth == 1 {
                // one byte: counting sort, stable
                let mut cnt = [0u32; 257];
                for &p in &scratch {
                    cnt[data[p as usize + depth] as usize + 1] += 1;
                }
                for b in 0..256 {
                    cnt[b + 1] += cnt[b];
                }
                for &p in &scratch {
                    let b = data[p as usize + depth] as usize;
                    group[cnt[b] as usize] = p;
                    cnt[b] += 1;
                }
            } else {
                scratch.sort_by(|&a, &b| {
                    data[a as usize + depth..a as usize + nd]
                        .cmp(&data[b as usize + depth..b as usize + nd])
                });
                group[..eligible].copy_from_slice(&scratch);
            }
            // walk the runs of equal keys
            let mut distinct = 0usize;
            let mut i = 0usize;
            while i < eligible {
                let key = &data[group[i] as usize + depth..group[i] as usize + nd];
                let mut j = i + 1;
                while j < eligible
                    && &data[group[j] as usize + depth..group[j] as usize + nd] == key
                {
                    j += 1;
                }
                distinct += 1;
                i = j;
            }
            let split = distinct > 1;
            let check_final = !split && nd < max_depth;
            let next_step = if split {
                step
            } else {
                (step * 2).min(MAX_STEP)
            };
            let mut i = 0usize;
            while i < eligible {
                let key_pos = group[i] as usize;
                let mut j = i + 1;
                while j < eligible
                    && data[group[j] as usize + depth..group[j] as usize + nd]
                        == data[key_pos + depth..key_pos + nd]
                {
                    j += 1;
                }
                if j - i >= 2 {
                    // record pairs, compact survivors to the front of the run
                    let mut prev = group[i] as usize;
                    let mut survivors = i + 1;
                    for k in i + 1..j {
                        let p = group[k] as usize;
                        let dist = (p - prev) as u32;
                        if dist != self.dists[p].load(Relaxed) {
                            // farther occurrence with a longer match: keep the nearer one,
                            // slot 0 = most recent, slot 1 = earliest longer than 2 bytes
                            let (ol, od) =
                                (self.lengths[p].load(Relaxed), self.dists[p].load(Relaxed));
                            if ol > 2 {
                                if self.alt_len[1][p].load(Relaxed) == 0 {
                                    self.alt_len[1][p].store(ol, Relaxed);
                                    self.alt_dist[1][p].store(od, Relaxed);
                                }
                                self.alt_len[0][p].store(ol, Relaxed);
                                self.alt_dist[0][p].store(od, Relaxed);
                            }
                            self.dists[p].store(dist, Relaxed);
                        }
                        if check_final
                            && p + max_depth <= n
                            && data[p..p + max_depth] == data[prev..prev + max_depth]
                        {
                            // already a full-depth match with its nearest occurrence: done
                            self.lengths[p].store(max_depth as u16, Relaxed);
                        } else {
                            self.lengths[p].store(nd as u16, Relaxed);
                            group[survivors] = p as u32;
                            survivors += 1;
                        }
                        prev = p;
                    }
                    if nd < max_depth && survivors - i > 1 {
                        stack.push((start + i, start + survivors, nd, next_step));
                    }
                }
                i = j;
            }
        }
    }
}

impl Matches for RadixMatchFinder<'_> {
    #[inline]
    fn at(&self, pos: usize) -> (u32, u32) {
        let len = self.lengths[pos].load(Relaxed) as usize;
        if len == 0 {
            return (0, 0);
        }
        let dist = self.dists[pos].load(Relaxed);
        let limit = (self.n - pos).min(MATCH_LEN_MAX as usize);
        if len >= limit {
            return (limit as u32, dist);
        }
        let ext = match_length(self.data, pos - dist as usize + len, pos + len, limit - len);
        ((len + ext) as u32, dist)
    }

    fn candidates(&self, pos: usize, out: &mut Vec<(u32, u32)>) {
        let deep = self.at(pos);
        if deep.0 == 0 {
            return;
        }
        out.push(deep);
        for k in 0..2 {
            let l = self.alt_len[k][pos].load(Relaxed) as u32;
            let d = self.alt_dist[k][pos].load(Relaxed);
            if l != 0 && !out.iter().any(|&(_, x)| x == d) {
                out.push((l, d));
            }
        }
        let nd = self.near[pos].load(Relaxed);
        if nd != 0 && !out.iter().any(|&(_, x)| x == nd) {
            let limit = (self.n - pos).min(MATCH_LEN_MAX as usize);
            let l = 2 + match_length(self.data, pos - nd as usize + 2, pos + 2, limit - 2);
            out.push((l as u32, nd));
        }
    }

    #[inline]
    fn probe(&self, pos: usize) -> u32 {
        self.lengths[pos].load(Relaxed) as u32
    }
}
