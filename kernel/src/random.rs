//! The kernel's random numbers: a ChaCha20 generator, seeded and
//! regularly reseeded from every source the machine has -- RDSEED and
//! RDRAND on the CPU (every PC from the last decade), virtio-rng under
//! QEMU (drivers/virtio/rng.rs), and as a last resort the jitter in how
//! long short loops take to run, read off the TSC. What the getrandom()
//! syscall hands out, what TLS keys are made from, and what seeds TCP
//! sequence numbers.
//!
//! Each fill() ends by replacing the key with fresh output ("fast key
//! erasure"), so nothing handed out earlier can be recomputed from the
//! state afterwards.

use crate::arch::x86_64::{cpuid, rdtsc};
use crate::drivers::{timer, virtio};
use crate::sync::Global;

/// Hardware sources are asked again at most this often.
const RESEED_MS: u64 = 1000;

struct Rng {
    key: [u32; 8],
    counter: u64,
    reseeded_ms: u64,
    seeded: bool,
    has_rdrand: bool,
    has_rdseed: bool,
}

static RNG: Global<Rng> =
    Global::new(Rng { key: [0; 8], counter: 0, reseeded_ms: 0, seeded: false, has_rdrand: false, has_rdseed: false });

fn rng() -> &'static mut Rng {
    unsafe { RNG.get() }
}

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// One ChaCha20 block (RFC 8439) for `key` at `counter`, nonce zero.
fn block(key: &[u32; 8], counter: u64) -> [u8; 64] {
    let mut init = [0u32; 16];
    init[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    init[4..12].copy_from_slice(key);
    init[12] = counter as u32;
    init[13] = (counter >> 32) as u32;
    let mut s = init;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
    out
}

fn rdrand() -> Option<u64> {
    let (v, ok): (u64, u8);
    unsafe { core::arch::asm!("rdrand {}; setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack)) };
    (ok != 0).then_some(v)
}

fn rdseed() -> Option<u64> {
    let (v, ok): (u64, u8);
    unsafe { core::arch::asm!("rdseed {}; setc {}", out(reg) v, out(reg_byte) ok, options(nomem, nostack)) };
    (ok != 0).then_some(v)
}

impl Rng {
    /// Folds `entropy` into the key, then stirs: the new key is ChaCha20
    /// output under the old one, so every input bit affects all of it.
    fn absorb(&mut self, entropy: &[u8]) {
        for (i, chunk) in entropy.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..chunk.len()].copy_from_slice(chunk);
            self.key[i % 8] ^= u32::from_le_bytes(w);
            if i % 8 == 7 {
                self.stir();
            }
        }
        self.stir();
    }

    fn stir(&mut self) {
        // A counter value fill() never reaches keeps this apart from output.
        let b = block(&self.key, u64::MAX);
        for i in 0..8 {
            self.key[i] = u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
        }
    }

    /// Asks every source for 32 bytes. Whether any good one answered.
    fn reseed(&mut self) -> bool {
        let mut good = false;
        let mut buf = [0u8; 32];
        for _ in 0..2 {
            for i in 0..4 {
                let w =
                    if self.has_rdseed { rdseed() } else { None }.or_else(|| self.has_rdrand.then(rdrand).flatten());
                if let Some(w) = w {
                    buf[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
                    good = true;
                }
            }
            self.absorb(&buf);
        }
        if virtio::rng::fill(&mut buf) == buf.len() {
            self.absorb(&buf);
            good = true;
        }
        // Always, and alone when nothing better exists: the low bits of
        // how long a few hundred short timed loops take.
        let mut jitter = [0u8; 256];
        for j in jitter.iter_mut() {
            let t0 = rdtsc();
            let mut x = t0;
            for k in 0..(t0 & 31) {
                x = x.rotate_left(7) ^ k;
            }
            *j = (rdtsc().wrapping_sub(t0) ^ x) as u8;
        }
        self.absorb(&jitter);
        self.reseeded_ms = timer::uptime_ms();
        good
    }
}

/// Finds the CPU's generators and seeds. After the drivers, so
/// virtio-rng can contribute.
pub fn init() {
    let r = rng();
    r.has_rdrand = (cpuid(1).2 >> 30) & 1 != 0;
    r.has_rdseed = cpuid(0).0 >= 7 && (cpuid(7).1 >> 18) & 1 != 0;
    let good = r.reseed();
    r.seeded = true;
    let mut sources = alloc::vec::Vec::new();
    if r.has_rdseed {
        sources.push("RDSEED");
    }
    if r.has_rdrand {
        sources.push("RDRAND");
    }
    if virtio::rng::is_ready() {
        sources.push("virtio-rng");
    }
    if good {
        kprintln!("random: seeded from {}", sources.join(", "));
    } else {
        kprintln!("random: WARNING -- no hardware source, seeded from timing jitter only (weak for TLS)");
    }
}

/// Fills `out` with random bytes.
pub fn fill(out: &mut [u8]) {
    let r = rng();
    if !r.seeded || timer::uptime_ms() - r.reseeded_ms > RESEED_MS {
        r.reseed();
        r.seeded = true;
    }
    for chunk in out.chunks_mut(64) {
        let b = block(&r.key, r.counter);
        r.counter += 1;
        chunk.copy_from_slice(&b[..chunk.len()]);
    }
    // Fast key erasure: the key that made `out` is gone.
    let b = block(&r.key, r.counter);
    r.counter += 1;
    for i in 0..8 {
        r.key[i] = u32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]);
    }
}

pub fn u64() -> u64 {
    let mut b = [0u8; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}
