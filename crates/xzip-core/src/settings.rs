//! Encoder settings and presets. Everything here is an encoder choice: any
//! combination produces valid .xz output.

use crate::error::{Error, Result};

pub const MATCH_LEN_MAX: u32 = 273;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Greedy with one step of lazy evaluation.
    Fast,
    /// Optimal parsing: price every alternative, take the cheapest path.
    Opt,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub strategy: Strategy,
    pub dict_size: u32,
    pub overlap: u32,     // sixteenths of the dictionary kept between buffers
    pub radix_depth: u32, // how deep the radix sort compares before the encoder extends
    pub radix_step: u32,  // bytes per radix level (1 = exact)
    pub nice_len: u32,    // a match this long is taken immediately
    pub lazy: bool,
    pub short_rep: bool,
    pub len3_max_dist: u32, // fast: 3-byte matches farther than this are not worth it
    pub rep_bias: u32,      // fast: prefer a rep up to this much shorter than a new match
    pub opt_len: u32,       // opt: how far ahead the optimal parser plans
    pub price_refresh: u32, // opt: symbols between price table rebuilds
    pub skip_incompressible: bool,
    pub skip_entropy: f64, // bits/byte above which a window is stored raw
    pub lc: u32,
    pub lp: u32,
    pub pb: u32,
    pub auto_props: bool, // pick lc/lp/pb by trial on a sample
    pub min_slice: usize, // smallest slice a thread encodes on its own
}

impl Default for Settings {
    fn default() -> Self {
        Settings::level(6)
    }
}

impl Settings {
    /// Levels 0-9, zstd/xz style. 0 is the quickest, 9 the smallest.
    pub fn level(level: u32) -> Settings {
        let level = level.min(9);
        let base = Settings {
            strategy: Strategy::Fast,
            dict_size: 1 << 23,
            overlap: 2,
            radix_depth: 32,
            radix_step: 1,
            nice_len: 64,
            lazy: true,
            short_rep: true,
            len3_max_dist: 1 << 12,
            rep_bias: 1,
            opt_len: 256,
            price_refresh: 256,
            skip_incompressible: true,
            skip_entropy: 7.9,
            lc: 3,
            lp: 0,
            pb: 2,
            auto_props: true,
            min_slice: 1 << 17,
        };
        match level {
            0 => Settings {
                dict_size: 1 << 18,
                radix_depth: 8,
                radix_step: 4,
                nice_len: 16,
                lazy: false,
                auto_props: false,
                ..base
            },
            1 => Settings {
                dict_size: 1 << 20,
                radix_depth: 12,
                radix_step: 4,
                nice_len: 32,
                lazy: false,
                ..base
            },
            2 => Settings {
                dict_size: 1 << 21,
                radix_depth: 16,
                radix_step: 2,
                nice_len: 32,
                ..base
            },
            3 => Settings {
                dict_size: 1 << 22,
                radix_depth: 24,
                nice_len: 48,
                ..base
            },
            4 => Settings {
                dict_size: 1 << 22,
                ..base
            },
            5 => Settings { ..base },
            6 => Settings {
                radix_depth: 48,
                strategy: Strategy::Opt,
                nice_len: 64,
                ..base
            },
            7 => Settings {
                dict_size: 1 << 24,
                radix_depth: 48,
                strategy: Strategy::Opt,
                nice_len: 128,
                opt_len: 512,
                ..base
            },
            8 => Settings {
                dict_size: 1 << 25,
                radix_depth: 64,
                strategy: Strategy::Opt,
                nice_len: 192,
                opt_len: 1024,
                ..base
            },
            _ => Settings {
                dict_size: 1 << 26,
                radix_depth: 96,
                strategy: Strategy::Opt,
                nice_len: 273,
                opt_len: 1024,
                price_refresh: 128,
                ..base
            },
        }
    }

    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::Settings(what.to_string()));
        if !(4096..=(1536 << 20)).contains(&self.dict_size) {
            return bad("dictionary size must be between 4 KiB and 1.5 GiB");
        }
        if self.overlap > 8 {
            return bad("overlap must be 0-8 sixteenths");
        }
        if !(2..=MATCH_LEN_MAX).contains(&self.radix_depth) {
            return bad("radix depth must be 2-273");
        }
        if !(1..=32).contains(&self.radix_step) {
            return bad("radix step must be 1-32");
        }
        if !(2..=MATCH_LEN_MAX).contains(&self.nice_len) {
            return bad("nice length must be 2-273");
        }
        if !(16..=4096).contains(&self.opt_len) {
            return bad("optimal parser window must be 16-4096");
        }
        if !(8..=65536).contains(&self.price_refresh) {
            return bad("price refresh must be 8-65536");
        }
        if !(1.0..=8.0).contains(&self.skip_entropy) {
            return bad("skip entropy must be 1.0-8.0");
        }
        if self.lc > 4 || self.lp > 4 || self.lc + self.lp > 4 || self.pb > 4 {
            return bad("lc, lp and pb must be 0-4 with lc + lp <= 4");
        }
        if self.min_slice < 4096 {
            return bad("minimum slice must be at least 4 KiB");
        }
        Ok(())
    }
}
