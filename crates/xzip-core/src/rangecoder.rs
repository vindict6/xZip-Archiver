//! Binary range coder, the entropy stage under LZMA. Every bit is coded against an
//! adaptive 11-bit probability; well-predicted bits cost almost nothing.
//! The constants are fixed by the LZMA format.

use std::sync::LazyLock;

pub const PROB_BITS: u32 = 11;
pub const PROB_ONE: u32 = 1 << PROB_BITS;
pub const PROB_INIT: u16 = (PROB_ONE / 2) as u16;
const MOVE_BITS: u32 = 5;
const TOP: u32 = 1 << 24;

/// Prices are in 1/16 bit, indexed by probability >> 4 (same idea as the LZMA SDK).
pub const PRICE_SHIFT: u32 = 4;
static PRICES: LazyLock<[u32; 128]> = LazyLock::new(|| {
    let mut t = [0u32; 128];
    for (i, slot) in t.iter_mut().enumerate() {
        let p = (i as f64 * 16.0 + 8.0) / PROB_ONE as f64;
        *slot = (-p.log2() * 16.0).round() as u32;
    }
    t
});

/// Cost in 1/16 bit of coding `bit` with probability `prob`.
#[inline]
pub fn price(prob: u16, bit: u32) -> u32 {
    let p = if bit != 0 {
        PROB_ONE - prob as u32
    } else {
        prob as u32
    };
    PRICES[((p >> PRICE_SHIFT) as usize).min(127)]
}

#[inline]
pub fn price0(prob: u16) -> u32 {
    PRICES[((prob as u32 >> PRICE_SHIFT) as usize).min(127)]
}

#[inline]
pub fn price1(prob: u16) -> u32 {
    PRICES[(((PROB_ONE - prob as u32) >> PRICE_SHIFT) as usize).min(127)]
}

pub struct RangeEncoder {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    out: Vec<u8>,
}

impl Default for RangeEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RangeEncoder {
    pub fn new() -> Self {
        RangeEncoder {
            low: 0,
            range: 0xFFFF_FFFF,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
        }
    }

    fn shift_low(&mut self) {
        // The top byte of `low` is settled unless it is 0xFF, in which case a later
        // carry could still bump it; 0xFF bytes are held back until that is known.
        if self.low < 0xFF00_0000 || self.low >= (1 << 32) {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }

    #[inline]
    pub fn encode_bit(&mut self, prob: &mut u16, bit: u32) {
        let p = *prob as u32;
        let bound = (self.range >> PROB_BITS) * p;
        if bit != 0 {
            self.low += bound as u64;
            self.range -= bound;
            *prob = (p - (p >> MOVE_BITS)) as u16;
        } else {
            self.range = bound;
            *prob = (p + ((PROB_ONE - p) >> MOVE_BITS)) as u16;
        }
        while self.range < TOP {
            self.range <<= 8;
            self.shift_low();
        }
    }

    pub fn encode_direct_bits(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            self.range >>= 1;
            if (value >> shift) & 1 != 0 {
                self.low += self.range as u64;
            }
            while self.range < TOP {
                self.range <<= 8;
                self.shift_low();
            }
        }
    }

    /// Code `value` high bit first through a binary tree of probabilities.
    pub fn encode_tree(&mut self, probs: &mut [u16], num_bits: u32, value: u32) {
        let mut m = 1usize;
        for shift in (0..num_bits).rev() {
            let bit = (value >> shift) & 1;
            self.encode_bit(&mut probs[m], bit);
            m = (m << 1) | bit as usize;
        }
    }

    pub fn encode_reverse_tree(&mut self, probs: &mut [u16], num_bits: u32, mut value: u32) {
        let mut m = 1usize;
        for _ in 0..num_bits {
            let bit = value & 1;
            value >>= 1;
            self.encode_bit(&mut probs[m], bit);
            m = (m << 1) | bit as usize;
        }
    }

    /// Size the output would have if finished now.
    pub fn pending_size(&self) -> usize {
        self.out.len() + self.cache_size as usize + 4
    }

    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

#[derive(Debug)]
pub struct CorruptRange;

pub struct RangeDecoder<'a> {
    buf: &'a [u8],
    pos: usize,
    code: u32,
    range: u32,
}

impl<'a> RangeDecoder<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self, CorruptRange> {
        if buf.len() < 5 || buf[0] != 0 {
            return Err(CorruptRange);
        }
        let code = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
        Ok(RangeDecoder {
            buf,
            pos: 5,
            code,
            range: 0xFFFF_FFFF,
        })
    }

    #[inline]
    fn normalize(&mut self) -> Result<(), CorruptRange> {
        if self.range < TOP {
            let b = *self.buf.get(self.pos).ok_or(CorruptRange)?;
            self.pos += 1;
            self.range <<= 8;
            self.code = (self.code << 8) | b as u32;
        }
        Ok(())
    }

    #[inline]
    pub fn decode_bit(&mut self, prob: &mut u16) -> Result<u32, CorruptRange> {
        let p = *prob as u32;
        let bound = (self.range >> PROB_BITS) * p;
        let bit = if self.code < bound {
            self.range = bound;
            *prob = (p + ((PROB_ONE - p) >> MOVE_BITS)) as u16;
            0
        } else {
            self.code -= bound;
            self.range -= bound;
            *prob = (p - (p >> MOVE_BITS)) as u16;
            1
        };
        self.normalize()?;
        Ok(bit)
    }

    pub fn decode_direct_bits(&mut self, count: u32) -> Result<u32, CorruptRange> {
        let mut result = 0u32;
        for _ in 0..count {
            self.range >>= 1;
            let bit = if self.code >= self.range {
                self.code -= self.range;
                1
            } else {
                0
            };
            result = (result << 1) | bit;
            self.normalize()?;
        }
        Ok(result)
    }

    pub fn decode_tree(&mut self, probs: &mut [u16], num_bits: u32) -> Result<u32, CorruptRange> {
        let mut m = 1usize;
        for _ in 0..num_bits {
            m = (m << 1) | self.decode_bit(&mut probs[m])? as usize;
        }
        Ok(m as u32 - (1 << num_bits))
    }

    pub fn decode_reverse_tree(
        &mut self,
        probs: &mut [u16],
        num_bits: u32,
    ) -> Result<u32, CorruptRange> {
        let mut m = 1usize;
        let mut result = 0u32;
        for i in 0..num_bits {
            let bit = self.decode_bit(&mut probs[m])?;
            m = (m << 1) | bit as usize;
            result |= bit << i;
        }
        Ok(result)
    }

    pub fn is_finished(&self) -> bool {
        self.code == 0 && self.pos == self.buf.len()
    }
}
