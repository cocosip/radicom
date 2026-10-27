//! PDU robustness baseline (docs/09-testing.md §7): arbitrary byte streams must
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
}

#[test]
fn arbitrary_pdu_bytes_never_panic() {
    let mut rng = Rng::new(0x00D1_CE01);
    for _ in 0..5_000 {
        let len = ((rng.next_u64() >> 32) as usize) % 64;
        let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
        let _ = decode(&bytes);
    }
}
