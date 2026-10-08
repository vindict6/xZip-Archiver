//! Pre-filters: our output must round-trip, and streams made by liblzma with each
//! filter must decode to the known input (tests/vectors, made with Python's lzma).

use xzip_core::filters::Filter;
use xzip_core::{xz, Check, FilterMode, Settings};

fn known_input() -> Vec<u8> {
    // same generator as the script that produced tests/vectors/*.xz
    let mut x: u64 = 3;
    let n = 6000;
    let mut out: Vec<u8> = (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    for i in (0..n - 8).step_by(13) {
        out[i] = [0xE8, 0xE9, 0xEB, 0x48, 0x40, 0x94, 0xF0][i % 7];
    }
    out
}

#[test]
fn generator_matches_vector_input() {
    assert_eq!(known_input(), include_bytes!("vectors/input.bin"));
}

#[test]
fn liblzma_filtered_streams_decode() {
    let input = known_input();
    let vectors: [(&str, &[u8], Filter); 9] = [
        ("x86", include_bytes!("vectors/x86.xz"), Filter::X86),
        ("arm64", include_bytes!("vectors/arm64.xz"), Filter::Arm64),
        ("arm", include_bytes!("vectors/arm.xz"), Filter::Arm),
        (
            "armthumb",
            include_bytes!("vectors/armthumb.xz"),
            Filter::ArmThumb,
        ),
        (
            "powerpc",
            include_bytes!("vectors/powerpc.xz"),
            Filter::PowerPc,
        ),
        ("sparc", include_bytes!("vectors/sparc.xz"), Filter::Sparc),
        (
            "delta1",
            include_bytes!("vectors/delta1.xz"),
            Filter::Delta(0),
        ),
        (
            "delta2",
            include_bytes!("vectors/delta2.xz"),
            Filter::Delta(1),
        ),
        (
            "delta4",
            include_bytes!("vectors/delta4.xz"),
            Filter::Delta(3),
        ),
    ];
    for (name, blob, filter) in vectors {
        let (out, info) =
            xz::decompress(blob, input.len()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(info.filter, filter, "{name}: filter id");
        assert_eq!(out, input, "{name}: contents");
    }
}

#[test]
fn our_filtered_streams_round_trip() {
    let input = known_input();
    let big: Vec<u8> = input.iter().cycle().take(300_000).copied().collect();
    for f in [
        Filter::X86,
        Filter::Arm64,
        Filter::Arm,
        Filter::ArmThumb,
        Filter::PowerPc,
        Filter::Sparc,
        Filter::Delta(0),
        Filter::Delta(3),
        Filter::Delta(255),
    ] {
        let s = Settings {
            filter: FilterMode::Fixed(f),
            auto_props: false,
            ..Settings::level(3)
        };
        let (blob, _, stats) = xz::compress(&big, &s, Check::Crc64, None).unwrap();
        assert_eq!(stats.filter, Some(f));
        let (out, info) = xz::decompress(&blob, big.len()).unwrap_or_else(|e| panic!("{f:?}: {e}"));
        assert_eq!(info.filter, f);
        assert_eq!(out, big, "{f:?}");
    }
}

#[test]
fn auto_mode_picks_x86_for_x86_code_and_nothing_for_text() {
    // a fake ELF x86-64 header followed by call-heavy "code": many CALLs to the same
    // few targets from different places only line up once the filter is applied
    let mut code = vec![0u8; 64];
    code[..4].copy_from_slice(b"\x7FELF");
    code[4] = 2;
    code[5] = 1;
    code[18] = 0x3E;
    let mut x: u32 = 7;
    while code.len() < 400_000 {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let filler = (x >> 16) as u8;
        code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5, filler, 0x90]);
        let target: u32 = 0x1000 + ((x >> 20) % 4) * 0x200;
        let rel = target.wrapping_sub(code.len() as u32 + 5);
        code.push(0xE8);
        code.extend_from_slice(&rel.to_le_bytes());
    }
    let s = Settings::level(3);
    assert_eq!(xz::choose_filter(&code, &s), Filter::X86);
    let text: Vec<u8> = b"the quick brown fox jumps over the lazy dog 0123456789\n"
        .iter()
        .cycle()
        .take(400_000)
        .copied()
        .collect();
    assert_eq!(xz::choose_filter(&text, &s), Filter::None);
    // and the archive-level API uses it
    let (blob, _, stats) = xz::compress(&code, &s, Check::Crc64, None).unwrap();
    assert_eq!(stats.filter, Some(Filter::X86));
    let (plain, _, _) = xz::compress(
        &code,
        &Settings {
            filter: FilterMode::Fixed(Filter::None),
            ..s.clone()
        },
        Check::Crc64,
        None,
    )
    .unwrap();
    assert!(
        blob.len() < plain.len(),
        "filter must help on call-heavy code: {} vs {}",
        blob.len(),
        plain.len()
    );
}
