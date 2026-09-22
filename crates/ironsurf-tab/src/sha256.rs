//! SHA-256 (FIPS 180-4) — the one digest this engine answers **natively**.
//!
//! `crypto.subtle.digest("SHA-256", …)` exists so that a page can hash at
//! wire speed instead of interpreting a JavaScript implementation. The engine
//! needs a real one for exactly one class of pages: **Anubis** proof-of-work
//! gates (startpage.com's search, and every other site that deploys
//! `anubis`, including the ones that hand the work to a Web Worker — see
//! [`crate::worker`]). A challenge asks for `floor(difficulty / 2)` leading
//! zero *bytes* of `SHA-256(randomData ‖ nonce)`, which is a loop of tens of
//! thousands of digests: an interpreter-bound implementation makes the page
//! sit at "Calculating…" for minutes, and a native one finishes in well under
//! a second.
//!
//! Only SHA-256 is implemented — deliberately. `crypto.subtle` is a large API
//! (AES, RSA-ECDSA, HKDF, PBKDF2, Ed25519, …) and pretending otherwise would
//! mean either shipping a crypto library through the wrong door or lying about
//! what the realm has: an AES-gcm seal that returns the input would be worse
//! than no `subtle` at all. Feature detection keeps working honestly because
//! `crypto.subtle.sign`/`encrypt`/`deriveBits` are absent, and
//! `crypto.subtle.digest` rejects every algorithm it does not implement
//! (`NotSupportedError`) rather than answering with something else's digest.
//!
//! This is a plain, dependency-free implementation — the 64 round constants,
//! the standard message schedule and the compression function, nothing more.
//! It is constant-time-ish by construction (no secret-dependent branches on
//! the message length) but it is **not** hardened: no side-channel
//! countermeasures, and `subtle`'s keyed operations are not here at all. It is
//! here to run challenges and checksums, not to protect keys.

use boa_engine::{Context, JsString, JsValue, NativeFunction};
use ironsurf_common::error;

/// The native's name in the realm (underscore-prefixed like every other
/// IronSurf binding, so it is obvious in a page stack trace).
pub const SHA256_NATIVE: &str = "__ironsurfSha256";

/// Round constants: the first 32 bits of the fractional parts of the cube
/// roots of the first 64 primes.
///
/// These are **computed from that definition** (Newton's method on the cube
/// root, 90 decimal digits, `floor(frac(cbrt(p)) * 2^32)`) rather than typed in
/// from a table: three of the values a transcription-prone human would
/// remember (`K[23]`, `K[38]`, `K[51]`) are wrong in the draft this replaced,
/// which is exactly the kind of bug a KAT catches and a reviewer does not.
/// The known-answer tests below pin the result against `sha256sum`.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Initial hash values: the first 32 bits of the fractional parts of the
/// square roots of the first 8 primes.
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// An in-progress SHA-256 computation: the running state, the partial block
/// and the total length in bits (the tail of the padding).
#[derive(Clone, Debug)]
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    buffered: usize,
    total_bits: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    #[must_use]
    pub fn new() -> Self {
        Sha256 {
            state: H0,
            block: [0; 64],
            buffered: 0,
            total_bits: 0,
        }
    }

    /// Feed more message bytes. Any whole 64-byte block is compressed as it
    /// fills; the remainder waits in `block`.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bits = self.total_bits.wrapping_add((data.len() as u64) * 8);
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(data.len());
            self.block[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 64 {
                let block = self.block;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        let mut chunks = data.chunks_exact(64);
        for chunk in &mut chunks {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            self.compress(&block);
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            self.block[..rest.len()].copy_from_slice(rest);
            self.buffered = rest.len();
        }
    }

    /// Absorb the padding (`0x80`, zeros, the 64-bit big-endian bit length)
    /// and hand back the 32-byte digest.
    #[must_use]
    pub fn finalize(mut self) -> [u8; 32] {
        let bits = self.total_bits.to_be_bytes();
        // The padding is `0x80` + zeros + the length; the final block must be
        // the one the length lands in, so a full-enough tail costs an extra
        // block (the classic `len % 64 > 55` case).
        let pad_len = if self.buffered < 56 { 64 - self.buffered } else { 128 - self.buffered };
        let mut padding = [0u8; 72];
        padding[0] = 0x80;
        padding[pad_len - 8..pad_len].copy_from_slice(&bits);
        self.update(&padding[..pad_len]);
        debug_assert_eq!(self.buffered, 0, "the padded message is a whole number of blocks");
        let mut out = [0u8; 32];
        for (word, slot) in self.state.iter().zip(out.chunks_exact_mut(4)) {
            slot.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// One-block digest (the whole message, no streaming) — what the native
    /// and every caller here uses.
    #[must_use]
    pub fn digest(data: &[u8]) -> [u8; 32] {
        let mut hasher = Self::new();
        hasher.update(data);
        hasher.finalize()
    }

    /// The compression function: 16 words of the block expanded to 64, then
    /// 64 rounds over the running state.
    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.chunks_exact(4)) {
            *word = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..64 {
            let s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
            let s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in self
            .state
            .iter_mut()
            .zip([a, b, c, d, e, f, g, h].into_iter())
        {
            *slot = slot.wrapping_add(value);
        }
    }
}

#[inline(always)]
fn rotr(value: u32, bits: u32) -> u32 {
    value.rotate_right(bits)
}

/// Lowercase hex of a digest (what Anubis sends as its `response=`).
#[must_use]
pub fn hex(digest: &[u8]) -> String {
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0F)]));
    }
    out
}

/// Which bytes the message text stands for: UTF-8 (what `TextEncoder`
/// produces, so `digest("SHA-256", new TextEncoder().encode(s))` reads the
/// same bytes) or Latin-1, one byte per UTF-16 code unit truncated to 8 bits
/// (what the glue uses to carry an arbitrary `BufferSource` through a JS
/// string without a byte-per-char JSON detour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMode {
    Utf8,
    Latin1,
}

/// The `__ironsurfSha256(text, mode)` native: the message in as a string, the
/// 32 digest bytes out as a Latin-1 string (one char per byte, codes ≤ 0xFF,
/// always valid UTF-16 — the same byte-string shape
/// `crypto.getRandomValues`'s native uses, which the glue reads back with
/// `charCodeAt`).
#[must_use]
pub fn native() -> NativeFunction {
    NativeFunction::from_copy_closure(|_this, args, context| {
        let text = args
            .first()
            .cloned()
            .unwrap_or(JsValue::undefined())
            .to_string(context)?
            .to_std_string_escaped();
        // Anything but an explicit `1` means UTF-8: the default is the mode
        // that matches `TextEncoder`, so a page that forgets the argument
        // still hashes the bytes it thinks it is hashing.
        let mode = match args.get(1).map(|value| value.to_u32(context)).transpose()? {
            Some(1) => TextMode::Latin1,
            _ => TextMode::Utf8,
        };
        let digest = match mode {
            TextMode::Utf8 => Sha256::digest(text.as_bytes()),
            TextMode::Latin1 => {
                let bytes: Vec<u8> = text.chars().map(|c| (c as u32 & 0xFF) as u8).collect();
                Sha256::digest(&bytes)
            }
        };
        let codes: Vec<u16> = digest.iter().map(|byte| u16::from(*byte)).collect();
        Ok(JsValue::from(JsString::from(String::from_utf16_lossy(&codes))))
    })
}

/// Install `__ironsurfSha256` on a realm (the tab realm and every worker
/// realm call this — the digest must answer identically in both).
pub fn register(context: &mut Context) {
    if let Err(err) =
        context.register_global_builtin_callable(JsString::from(SHA256_NATIVE), 2, native())
    {
        error!("js binding `{SHA256_NATIVE}` failed to install: {err}");
    }
}

// ---------------------------------------------------------------------------
// Unit tests

#[cfg(test)]
mod tests {
    use super::*;

    fn digest_of(text: &str) -> String {
        hex(&Sha256::digest(text.as_bytes()))
    }

    /// The FIPS 180-4 / well-known vectors. Every expected value here was
    /// produced by `sha256sum` (coreutils), i.e. an independent implementation
    /// — including the two lengths that straddle a block boundary (55 bytes is
    /// the last message whose padding fits in the first block, 56 is the first
    /// that needs a second) and the million-`a` case from the standard.
    #[test]
    fn known_answer_tests() {
        assert_eq!(
            digest_of(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            digest_of("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_of(&"a".repeat(55)),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            digest_of(&"a".repeat(56)),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            digest_of(&"a".repeat(64)),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        assert_eq!(
            digest_of(&"a".repeat(1000)),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn multi_block_message_vectors() {
        // A running alphabet just over two blocks: the message schedule runs
        // past its first expansion, and the tail is a partial block.
        let message = "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnopnopinqrsstuuvuvv";
        assert_eq!(message.len(), 65);
        assert_eq!(
            digest_of(message),
            "788bed1da30a6df907bd1f91204e0644be2f64563bf7022aeb9e77a5df6456cb"
        );
    }

    #[test]
    fn streaming_matches_one_shot() {
        let message: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        let one_shot = Sha256::digest(&message);
        let mut streamed = Sha256::new();
        let mut offset = 0;
        while offset < message.len() {
            let end = (offset + 7).min(message.len());
            streamed.update(&message[offset..end]);
            offset = end;
        }
        assert_eq!(streamed.finalize(), one_shot);
    }

    #[test]
    fn million_a() {
        // The standard's example 2: SHA-256 of one million `a`.
        let mut hasher = Sha256::new();
        for _ in 0..1000 {
            hasher.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&hasher.finalize()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn latin1_mode_reads_one_byte_per_char() {
        // "é" as U+00E9 must hash as the single byte 0xE9 in Latin-1 mode, and
        // as its two UTF-8 bytes otherwise.
        let latin = |text: &str| {
            let bytes: Vec<u8> = text.chars().map(|c| c as u32 as u8).collect();
            hex(&Sha256::digest(&bytes))
        };
        assert_eq!(latin("é"), hex(&Sha256::digest(&[0xE9])));
        assert_eq!(
            latin("é"),
            "de2e331d891ae267a7009cb45b4e8830f170e0c937288ea2731a1941c7a53b0d"
        );
        assert_eq!(
            digest_of("é"),
            "4a99557e4033c3539de2eb65472017cad5f9557f7a0625a09f1c3f6e2ba69c4c",
            "UTF-8 mode must hash the two bytes of U+00E9"
        );
        assert_ne!(latin("é"), digest_of("é"));
    }

    /// The shape a challenge loop actually runs: find a nonce whose digest has
    /// `difficulty / 2` leading zero bytes (Anubis' "fast" algorithm).
    #[test]
    fn proof_of_work_shape() {
        let random_data = "anubis-random-data-0123456789";
        let difficulty = 4u32;
        let zero_bytes = difficulty as usize / 2;
        let mut nonce = 0u64;
        loop {
            let message = format!("{random_data}{nonce}");
            let digest = Sha256::digest(message.as_bytes());
            if digest[..zero_bytes].iter().all(|byte| *byte == 0)
                && (difficulty % 2 == 0 || (digest[zero_bytes] >> 4) == 0)
            {
                break;
            }
            nonce += 1;
        }
        assert!(nonce > 0, "the loop must actually search");
    }
}
