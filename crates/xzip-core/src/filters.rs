//! Pre-filters that make machine code (and some binary tables) compress better.
//!
//! A CALL on x86 stores a *relative* target, so the same function called from a
//! hundred places looks like a hundred different byte patterns. The branch/call/jump
//! ("BCJ") filters rewrite those relative targets as absolute ones before LZMA sees
//! the data, so the repeats line up; the decoder undoes it afterwards. The Delta
//! filter subtracts the byte N positions back, which flattens slowly changing
//! interleaved data (raw audio, some image rows, tables).
//!
//! These are the same filters as in the .xz format, bit for bit, so filtered
//! streams still open in xz and 7-Zip. Filter IDs and semantics follow liblzma.

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    None,
    X86,
    PowerPc,
    Arm,
    ArmThumb,
    Sparc,
    Arm64,
    /// Subtract the byte `distance` positions back (1..=256).
    Delta(u8),
}

impl Filter {
    pub fn id(self) -> u64 {
        match self {
            Filter::None => 0,
            Filter::Delta(_) => 0x03,
            Filter::X86 => 0x04,
            Filter::PowerPc => 0x05,
            Filter::Arm => 0x07,
            Filter::ArmThumb => 0x08,
            Filter::Sparc => 0x09,
            Filter::Arm64 => 0x0A,
        }
    }

    pub fn from_id(id: u64, props: &[u8]) -> Result<Filter> {
        Ok(match (id, props.len()) {
            (0x03, 1) => Filter::Delta(props[0]),
            (0x04, 0) => Filter::X86,
            (0x05, 0) => Filter::PowerPc,
            (0x07, 0) => Filter::Arm,
            (0x08, 0) => Filter::ArmThumb,
            (0x09, 0) => Filter::Sparc,
            (0x0A, 0) => Filter::Arm64,
            (0x04 | 0x05 | 0x07 | 0x08 | 0x09 | 0x0A, 4) => {
                return Err(Error::Corrupt(
                    "BCJ filters with a start offset are not supported".into(),
                ))
            }
            (0x06, _) => return Err(Error::Corrupt("the IA-64 filter is not supported".into())),
            (0x0B, _) => return Err(Error::Corrupt("the RISC-V filter is not supported".into())),
            _ => return Err(Error::Corrupt(format!("unknown filter 0x{id:02X}"))),
        })
    }

    /// Properties bytes as stored in the .xz block header.
    pub fn props(self) -> Vec<u8> {
        match self {
            Filter::Delta(d) => vec![d],
            _ => Vec::new(),
        }
    }

    pub fn name(self) -> String {
        match self {
            Filter::None => "none".into(),
            Filter::X86 => "x86".into(),
            Filter::PowerPc => "powerpc".into(),
            Filter::Arm => "arm".into(),
            Filter::ArmThumb => "armthumb".into(),
            Filter::Sparc => "sparc".into(),
            Filter::Arm64 => "arm64".into(),
            Filter::Delta(d) => format!("delta:{}", d as u32 + 1),
        }
    }

    pub fn parse(s: &str) -> Option<Filter> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "none" | "off" => Filter::None,
            "x86" | "bcj" => Filter::X86,
            "powerpc" | "ppc" => Filter::PowerPc,
            "arm" => Filter::Arm,
            "armthumb" | "thumb" => Filter::ArmThumb,
            "sparc" => Filter::Sparc,
            "arm64" | "aarch64" => Filter::Arm64,
            _ => {
                let d: u32 = s
                    .strip_prefix("delta:")
                    .or(s.strip_prefix("delta"))?
                    .parse()
                    .ok()?;
                if !(1..=256).contains(&d) {
                    return None;
                }
                Filter::Delta((d - 1) as u8)
            }
        })
    }

    /// Apply the filter in place (encoding direction).
    pub fn encode(self, data: &mut [u8]) {
        self.run(data, true);
    }

    /// Undo the filter in place.
    pub fn decode(self, data: &mut [u8]) {
        self.run(data, false);
    }

    fn run(self, data: &mut [u8], enc: bool) {
        match self {
            Filter::None => {}
            Filter::X86 => x86(data, enc),
            Filter::PowerPc => powerpc(data, enc),
            Filter::Arm => arm(data, enc),
            Filter::ArmThumb => armthumb(data, enc),
            Filter::Sparc => sparc(data, enc),
            Filter::Arm64 => arm64(data, enc),
            Filter::Delta(d) => delta(data, d as usize + 1, enc),
        }
    }
}

#[inline]
fn test86_ms_byte(b: u8) -> bool {
    b == 0 || b == 0xFF
}

/// x86 E8/E9 (CALL/JMP rel32). The mask tracks recent E8/E9 bytes so that runs of
/// them (which are data, not code) are left alone, exactly as liblzma does.
fn x86(buf: &mut [u8], enc: bool) {
    const MASK_TO_ALLOWED: [bool; 8] = [true, true, true, false, true, false, false, false];
    const MASK_TO_BIT_NUMBER: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];
    let size = buf.len();
    if size < 5 {
        return;
    }
    let mut prev_mask: u32 = 0;
    let mut prev_pos: u32 = 0u32.wrapping_sub(5);
    let limit = size - 5;
    let mut pos = 0usize;
    while pos <= limit {
        let b = buf[pos];
        if b != 0xE8 && b != 0xE9 {
            pos += 1;
            continue;
        }
        let offset = (pos as u32).wrapping_sub(prev_pos);
        prev_pos = pos as u32;
        if offset > 5 {
            prev_mask = 0;
        } else {
            for _ in 0..offset {
                prev_mask &= 0x77;
                prev_mask <<= 1;
            }
        }
        let b4 = buf[pos + 4];
        if test86_ms_byte(b4)
            && MASK_TO_ALLOWED[((prev_mask >> 1) & 7) as usize]
            && (prev_mask >> 1) < 0x10
        {
            let mut src = (b4 as u32) << 24
                | (buf[pos + 3] as u32) << 16
                | (buf[pos + 2] as u32) << 8
                | buf[pos + 1] as u32;
            let mut dest;
            loop {
                let cur = (pos as u32).wrapping_add(5);
                dest = if enc {
                    src.wrapping_add(cur)
                } else {
                    src.wrapping_sub(cur)
                };
                if prev_mask == 0 {
                    break;
                }
                let i = MASK_TO_BIT_NUMBER[(prev_mask >> 1) as usize];
                let b = (dest >> (24 - i * 8)) as u8;
                if !test86_ms_byte(b) {
                    break;
                }
                src = dest ^ ((1u32 << (32 - i * 8)).wrapping_sub(1));
            }
            buf[pos + 4] = (!(((dest >> 24) & 1).wrapping_sub(1))) as u8;
            buf[pos + 3] = (dest >> 16) as u8;
            buf[pos + 2] = (dest >> 8) as u8;
            buf[pos + 1] = dest as u8;
            pos += 5;
            prev_mask = 0;
        } else {
            pos += 1;
            prev_mask |= 1;
            if test86_ms_byte(b4) {
                prev_mask |= 0x10;
            }
        }
    }
}

fn arm64(buf: &mut [u8], enc: bool) {
    let mut i = 0;
    while i + 4 <= buf.len() {
        let mut pc = i as u32;
        let mut instr = u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        if instr >> 26 == 0x25 {
            // BL
            let src = instr;
            instr = 0x9400_0000;
            pc >>= 2;
            if !enc {
                pc = 0u32.wrapping_sub(pc);
            }
            instr |= src.wrapping_add(pc) & 0x03FF_FFFF;
            buf[i..i + 4].copy_from_slice(&instr.to_le_bytes());
        } else if instr & 0x9F00_0000 == 0x9000_0000 {
            // ADRP
            let src = ((instr >> 29) & 3) | ((instr >> 3) & 0x001F_FFFC);
            if (src.wrapping_add(0x0002_0000)) & 0x001C_0000 == 0 {
                instr &= 0x9000_001F;
                pc >>= 12;
                if !enc {
                    pc = 0u32.wrapping_sub(pc);
                }
                let dest = src.wrapping_add(pc);
                instr |= (dest & 3) << 29;
                instr |= (dest & 0x0003_FFFC) << 3;
                instr |= (0u32.wrapping_sub(dest & 0x0002_0000)) & 0x00E0_0000;
                buf[i..i + 4].copy_from_slice(&instr.to_le_bytes());
            }
        }
        i += 4;
    }
}

fn arm(buf: &mut [u8], enc: bool) {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i + 3] == 0xEB {
            let src = ((buf[i + 2] as u32) << 16 | (buf[i + 1] as u32) << 8 | buf[i] as u32) << 2;
            let cur = (i as u32).wrapping_add(8);
            let dest = (if enc {
                src.wrapping_add(cur)
            } else {
                src.wrapping_sub(cur)
            }) >> 2;
            buf[i + 2] = (dest >> 16) as u8;
            buf[i + 1] = (dest >> 8) as u8;
            buf[i] = dest as u8;
        }
        i += 4;
    }
}

fn armthumb(buf: &mut [u8], enc: bool) {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i + 1] & 0xF8 == 0xF0 && buf[i + 3] & 0xF8 == 0xF8 {
            let src = (((buf[i + 1] as u32) & 7) << 19
                | (buf[i] as u32) << 11
                | ((buf[i + 3] as u32) & 7) << 8
                | buf[i + 2] as u32)
                << 1;
            let cur = (i as u32).wrapping_add(4);
            let dest = (if enc {
                src.wrapping_add(cur)
            } else {
                src.wrapping_sub(cur)
            }) >> 1;
            buf[i + 1] = 0xF0 | ((dest >> 19) & 7) as u8;
            buf[i] = (dest >> 11) as u8;
            buf[i + 3] = 0xF8 | ((dest >> 8) & 7) as u8;
            buf[i + 2] = dest as u8;
            i += 2;
        }
        i += 2;
    }
}

fn powerpc(buf: &mut [u8], enc: bool) {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i] >> 2 == 0x12 && buf[i + 3] & 3 == 1 {
            let src = ((buf[i] as u32) & 3) << 24
                | (buf[i + 1] as u32) << 16
                | (buf[i + 2] as u32) << 8
                | ((buf[i + 3] as u32) & !3);
            let cur = i as u32;
            let dest = if enc {
                src.wrapping_add(cur)
            } else {
                src.wrapping_sub(cur)
            };
            buf[i] = 0x48 | ((dest >> 24) & 3) as u8;
            buf[i + 1] = (dest >> 16) as u8;
            buf[i + 2] = (dest >> 8) as u8;
            buf[i + 3] = (buf[i + 3] & 3) | (dest as u8 & !3);
        }
        i += 4;
    }
}

fn sparc(buf: &mut [u8], enc: bool) {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if (buf[i] == 0x40 && buf[i + 1] & 0xC0 == 0x00)
            || (buf[i] == 0x7F && buf[i + 1] & 0xC0 == 0xC0)
        {
            let src = u32::from_be_bytes(buf[i..i + 4].try_into().unwrap()) << 2;
            let cur = i as u32;
            let mut dest = (if enc {
                src.wrapping_add(cur)
            } else {
                src.wrapping_sub(cur)
            }) >> 2;
            dest = (((0u32.wrapping_sub((dest >> 22) & 1)) << 22) & 0x3FFF_FFFF)
                | (dest & 0x003F_FFFF)
                | 0x4000_0000;
            buf[i..i + 4].copy_from_slice(&dest.to_be_bytes());
        }
        i += 4;
    }
}

fn delta(buf: &mut [u8], dist: usize, enc: bool) {
    if enc {
        for i in (dist..buf.len()).rev() {
            buf[i] = buf[i].wrapping_sub(buf[i - dist]);
        }
    } else {
        for i in dist..buf.len() {
            buf[i] = buf[i].wrapping_add(buf[i - dist]);
        }
    }
}

// ---- Choosing a filter ---------------------------------------------------------------

/// What the file header says the code is for, if it is an executable at all.
pub fn sniff_executable(data: &[u8]) -> Option<Filter> {
    if data.len() >= 20 && data.starts_with(b"\x7FELF") {
        let le = data[5] == 1;
        let machine = if le {
            u16::from_le_bytes([data[18], data[19]])
        } else {
            u16::from_be_bytes([data[18], data[19]])
        };
        return match machine {
            0x03 | 0x3E => Some(Filter::X86),
            0xB7 => Some(Filter::Arm64),
            0x28 => Some(Filter::ArmThumb),
            0x14 | 0x15 => Some(Filter::PowerPc),
            0x02 | 0x12 | 0x2B => Some(Filter::Sparc),
            _ => None,
        };
    }
    if data.len() >= 0x40 && data.starts_with(b"MZ") {
        let off = u32::from_le_bytes(data[0x3C..0x40].try_into().unwrap()) as usize;
        if data.len() >= off + 6 && &data[off..off + 4] == b"PE\0\0" {
            return match u16::from_le_bytes([data[off + 4], data[off + 5]]) {
                0x014C | 0x8664 => Some(Filter::X86),
                0xAA64 => Some(Filter::Arm64),
                0x01C0 | 0x01C2 | 0x01C4 => Some(Filter::ArmThumb),
                _ => None,
            };
        }
        return None;
    }
    if data.len() >= 8 {
        let magic = u32::from_le_bytes(data[0..4].try_into().unwrap());
        if magic == 0xFEED_FACF || magic == 0xFEED_FACE {
            let cpu = u32::from_le_bytes(data[4..8].try_into().unwrap()) & 0x00FF_FFFF;
            return match cpu {
                7 => Some(Filter::X86),
                12 => Some(Filter::Arm64),
                _ => None,
            };
        }
        if magic == 0xCFFA_EDFE || magic == 0xCEFA_EDFE {
            let cpu = u32::from_be_bytes(data[4..8].try_into().unwrap()) & 0x00FF_FFFF;
            return match cpu {
                7 => Some(Filter::X86),
                12 => Some(Filter::Arm64),
                _ => None,
            };
        }
    }
    None
}

/// Candidates worth trying when the header says nothing.
pub const TRIAL_FILTERS: [Filter; 10] = [
    Filter::X86,
    Filter::Arm64,
    Filter::ArmThumb,
    Filter::Arm,
    Filter::PowerPc,
    Filter::Sparc,
    Filter::Delta(0),
    Filter::Delta(1),
    Filter::Delta(2),
    Filter::Delta(3),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo_random(n: usize, mut x: u64) -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn every_filter_is_a_bijection() {
        for seed in 1..6u64 {
            let mut data = pseudo_random(10_000 + seed as usize * 7, seed);
            // sprinkle plausible opcodes so the branches actually fire
            for i in (0..data.len() - 8).step_by(13) {
                data[i] = [0xE8, 0xE9, 0xEB, 0x48, 0x40, 0x94, 0xF0][i % 7];
                data[i + 4] = 0; // x86 only converts when the high byte is 00 or FF
            }
            for f in TRIAL_FILTERS.iter().copied().chain([Filter::Delta(255)]) {
                let mut buf = data.clone();
                f.encode(&mut buf);
                assert_ne!(buf, data, "{f:?} did nothing");
                f.decode(&mut buf);
                assert_eq!(buf, data, "{f:?} round trip");
            }
        }
    }

    #[test]
    fn sniffing() {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7FELF");
        elf[5] = 1;
        elf[18] = 0x3E;
        assert_eq!(sniff_executable(&elf), Some(Filter::X86));
        elf[18] = 0xB7;
        assert_eq!(sniff_executable(&elf), Some(Filter::Arm64));
        let mut pe = vec![0u8; 0x100];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3C] = 0x80;
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84] = 0x64;
        pe[0x85] = 0x86;
        assert_eq!(sniff_executable(&pe), Some(Filter::X86));
        assert_eq!(sniff_executable(b"hello world, not an executable"), None);
        assert_eq!(Filter::parse("delta:4"), Some(Filter::Delta(3)));
        assert_eq!(Filter::parse("x86"), Some(Filter::X86));
    }
}
