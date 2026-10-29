//! Robustness / fuzz-style baselines (docs/02-development-guide.md §5 §7).
//!
//! Pure-`std` deterministic byte sweeps (no cargo-fuzz / libfuzzer needed): the
//! reader must terminate with a typed `Result` — never panic or hang — on
//! arbitrary or mutated input, and a bogus huge declared length must be rejected
//! by `require` rather than turned into a giant allocation.

use std::path::PathBuf;

use radicom::io::dataset_builder::parse_dataset_bytes;
use radicom::io::file::{DicomFile, ReadOptions};
use radicom::model::transfer_syntax::syntaxes;

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() >> 32) as u32 % n
    }
}

fn corpus(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests-data")
        .join(rel)
}

#[test]
fn arbitrary_bytes_never_panic() {
    let syntaxes = [
        syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone(),
        syntaxes::IMPLICIT_VR_LITTLE_ENDIAN.clone(),
        syntaxes::EXPLICIT_VR_BIG_ENDIAN.clone(),
        syntaxes::DEFLATED_EXPLICIT_VR_LITTLE_ENDIAN.clone(),
    ];
    let mut rng = Rng::new(0x00C0_FFEE);
    for _ in 0..2_000 {
        let len = rng.below(300) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        for syntax in &syntaxes {
            let _ = parse_dataset_bytes(&bytes, syntax);
        }
        let _ = DicomFile::from_bytes(&bytes, &ReadOptions::default());
    }
}

#[test]
fn mutated_valid_file_never_panics() {
    let original = std::fs::read(corpus("explicit-le/GH487.dcm")).expect("read GH487.dcm");
    let mut rng = Rng::new(0x00BE_EF11);
    for _ in 0..1_000 {
        let mut bytes = original.clone();
        let flips = 1 + rng.below(4) as usize;
        for _ in 0..flips {
            let pos = rng.below(bytes.len() as u32) as usize;
            bytes[pos] ^= 1u8 << rng.below(8);
        }
        let _ = DicomFile::from_bytes(&bytes, &ReadOptions::default());
    }
}

#[test]
fn declared_huge_length_is_rejected_not_allocated() {
    // (7FE0,0010) OW with a ~4 GiB declared length and no payload: `require`
    // must reject it via EOF, not attempt a 4 GiB allocation.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x7FE0u16.to_le_bytes());
    bytes.extend_from_slice(&0x0010u16.to_le_bytes());
    bytes.extend_from_slice(b"OW");
    bytes.extend_from_slice(&0u16.to_le_bytes()); // reserved
    bytes.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());

    let syntax = syntaxes::EXPLICIT_VR_LITTLE_ENDIAN.clone();
    assert!(
        parse_dataset_bytes(&bytes, &syntax).is_err(),
        "a huge declared length on a short buffer must be a typed error"
    );
}
