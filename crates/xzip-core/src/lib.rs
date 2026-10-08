//! xzip-core: a Fast-LZMA2 style .xz compressor and the .xzip archive format.
//!
//! Copyright (c) Clinton Turner.
//!
//! Pipeline: radix match finder -> fast or optimal parser -> LZMA model -> range
//! coder -> LZMA2 chunks -> .xz container -> .xzip archive. No unsafe code anywhere
//! in this crate; untrusted input is bounds-checked, never trusted for allocation
//! sizes, and refused before it is parsed when a hash does not match.

#![forbid(unsafe_code)]
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]

pub mod archive;
pub mod error;
pub mod lzma;
pub mod lzma2;
pub mod optimal;
pub mod paths;
pub mod radix;
pub mod rangecoder;
pub mod settings;
pub mod xz;

pub use archive::{
    create_archive, load_archive, open_archive, rebuild_archive, Archive, ArchiveProgress,
    CreateOptions, Entry, ExtractOptions, ExtractReport, Overwrite, Skipped,
};
pub use error::{Error, Result};
pub use settings::{Settings, Strategy};
pub use xz::Check;

/// Human-readable size.
pub fn format_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_text() -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..4000 {
            v.extend_from_slice(
                format!(
                    "line {i}: the quick brown fox jumps over the lazy dog {}\n",
                    i % 17
                )
                .as_bytes(),
            );
        }
        v
    }

    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn decode_with_lzma_rs(xz: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        lzma_rs::xz_decompress(&mut std::io::Cursor::new(xz), &mut out)
            .expect("lzma-rs decodes our output");
        out
    }

    fn roundtrip(data: &[u8], s: &Settings) {
        let (xz, _, _) = xz::compress(data, s, Check::Crc64, None).unwrap();
        assert_eq!(decode_with_lzma_rs(&xz), data, "lzma-rs round trip");
        let (ours, _) = xz::decompress(&xz, data.len().max(1)).unwrap();
        assert_eq!(ours, data, "own decoder round trip");
    }

    #[test]
    fn roundtrips_all_levels() {
        let text = sample_text();
        let rnd = pseudo_random(200_000, 7);
        let zeros = vec![0u8; 300_000];
        let periodic: Vec<u8> = b"000X".iter().cycle().take(200_000).copied().collect();
        let mut mixed = rnd[..30_000].to_vec();
        mixed.extend_from_slice(&text);
        mixed.extend_from_slice(&zeros[..70_000]);
        let struct32: Vec<u8> = (0..60000u32)
            .step_by(3)
            .flat_map(|i| i.to_le_bytes())
            .collect();
        for level in [0, 1, 3, 5, 6, 7, 9] {
            let s = Settings::level(level);
            for d in [
                &b""[..],
                b"A",
                b"AB",
                b"ABC",
                &text,
                &rnd,
                &zeros,
                &periodic,
                &mixed,
                &struct32,
            ] {
                roundtrip(d, &s);
            }
        }
    }

    #[test]
    fn roundtrips_odd_settings() {
        let text = sample_text();
        for s in [
            Settings {
                dict_size: 1 << 18,
                overlap: 0,
                ..Settings::level(4)
            },
            Settings {
                dict_size: 1 << 17,
                overlap: 4,
                lc: 2,
                lp: 2,
                pb: 3,
                auto_props: false,
                ..Settings::level(6)
            },
            Settings {
                min_slice: 4096,
                ..Settings::level(2)
            },
            Settings {
                radix_step: 4,
                radix_depth: 8,
                ..Settings::level(5)
            },
        ] {
            let big: Vec<u8> = text.iter().cycle().take(900_000).copied().collect();
            roundtrip(&big, &s);
        }
    }

    #[test]
    fn decodes_foreign_streams() {
        let text = sample_text();
        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut std::io::Cursor::new(&text), &mut xz).unwrap();
        let (ours, _) = xz::decompress(&xz, text.len()).unwrap();
        assert_eq!(ours, text);
    }

    #[test]
    fn corrupt_streams_are_refused() {
        let text = sample_text();
        let (xz, _, _) = xz::compress(&text, &Settings::level(3), Check::Crc64, None).unwrap();
        for i in [5, 20, xz.len() / 2, xz.len() - 10] {
            let mut bad = xz.clone();
            bad[i] ^= 1;
            assert!(
                xz::decompress(&bad, text.len()).is_err(),
                "flipped byte {i}"
            );
        }
        assert!(xz::decompress(&xz[..xz.len() - 1], text.len()).is_err());
        assert!(xz::decompress(&xz, text.len() - 1).is_err(), "output cap");
    }

    #[test]
    fn incompressible_data_is_stored() {
        let rnd = pseudo_random(300_000, 3);
        let (xz, _, st) = xz::compress(&rnd, &Settings::level(5), Check::Crc64, None).unwrap();
        assert!(xz.len() < rnd.len() + 200);
        assert!(st.skipped_bytes > 0, "entropy probe should skip parsing");
        let mut dup = rnd[..150_000].to_vec();
        dup.extend_from_slice(&rnd[..150_000]);
        let (xz2, _, _) = xz::compress(&dup, &Settings::level(5), Check::Crc64, None).unwrap();
        assert!(
            xz2.len() < 160_000,
            "copy of earlier data must still be matched: {}",
            xz2.len()
        );
        assert_eq!(decode_with_lzma_rs(&xz2), dup);
    }

    #[test]
    fn format_size_works() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(2048), "2.0 KiB");
    }
}
