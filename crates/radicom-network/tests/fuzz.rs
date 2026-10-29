//! PDU robustness baseline (docs/02-development-guide.md §5.7): arbitrary byte streams must
//! decode to a typed `Result` without panicking. Pure `std`, deterministic.

use radicom_network::pdu::decode;

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
    fn below(&mut self, bound: usize) -> usize {
        ((self.next_u64() >> 32) as usize) % bound
    }
}

#[test]
fn arbitrary_pdu_bytes_never_panic() {
    let mut rng = Rng::new(0x00D1_CE01);
    for i in 0..5_000 {
        // short frames exercise the header/framing checks; frames of 68+ bytes
        // are the smallest that can carry an A-ASSOCIATE body (review gap: the
        // old `% 64` never reached `decode_associate`).
        let len = if i % 2 == 0 {
            rng.below(64)
        } else {
            68 + rng.below(232)
        };
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        let _ = decode(&bytes);
    }
}

#[test]
fn arbitrary_associate_bodies_never_panic() {
    // Force the A-ASSOCIATE-RQ type and a consistent body length so every
    // iteration runs `decode_associate`, its presentation-context decoder and
    // the user-information sub-item decoder on arbitrary bytes.
    let mut rng = Rng::new(0x00A5_50C1);
    for _ in 0..2_000 {
        let body_len = 68 + rng.below(400);
        let mut bytes: Vec<u8> = (0..body_len).map(|_| rng.next_u64() as u8).collect();
        bytes[0] = 0x01; // A-ASSOCIATE-RQ
        bytes[1] = 0;
        bytes[2..6].copy_from_slice(&(body_len as u32).to_be_bytes());
        let _ = decode(&bytes);
    }
}
