//! Pairing: a one-time code shown by the agent, typed on the PC, that
//! authenticates the agent's certificate both ways (see the design doc).

use ring::digest::{SHA256, digest};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

/// Code alphabet: no 0/O, 1/I/L, U (Crockford-ish), so codes survive being read aloud.
const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ%#";
/// 8 characters of 5 bits.
pub const CODE_LEN: usize = 8;

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    SystemRandom::new()
        .fill(&mut b)
        .expect("the system's random number generator failed");
    b
}

/// A new pairing code, `XXXX-XXXX`.
pub fn new_code() -> String {
    // Only the first 30 letters: '%' and '#' are never generated, they only
    // round the alphabet up to 32 entries.
    let bytes: [u8; CODE_LEN] = random();
    let mut s = String::with_capacity(CODE_LEN + 1);
    for (i, b) in bytes.iter().enumerate() {
        if i == 4 {
            s.push('-');
        }
        s.push(ALPHABET[(*b as usize) % 30] as char);
    }
    s
}

/// A code as typed: case, spaces and dashes don't matter.
pub fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn proof(code: &str, who: &[u8], fingerprint: &[u8; 32], nonce: &[u8; 16]) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, normalize_code(code).as_bytes());
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(b"quickseek-pair-v1\0");
    ctx.update(who);
    ctx.update(fingerprint);
    ctx.update(nonce);
    ctx.sign().as_ref().try_into().unwrap()
}

/// What the PC sends: proves it knows the code and which certificate it saw.
pub fn pc_proof(code: &str, fingerprint: &[u8; 32], nonce: &[u8; 16]) -> [u8; 32] {
    proof(code, b"pc", fingerprint, nonce)
}

/// What the agent answers: proves the agent knows the code too.
pub fn agent_proof(code: &str, fingerprint: &[u8; 32], nonce: &[u8; 16]) -> [u8; 32] {
    proof(code, b"agent", fingerprint, nonce)
}

/// Constant-time comparison.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    digest(&SHA256, data).as_ref().try_into().unwrap()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A fingerprint for people: `SHA256:ab:cd:…` style, grouped.
pub fn show_fingerprint(fp: &[u8; 32]) -> String {
    let h = hex(fp);
    let groups: Vec<&str> = (0..h.len())
        .step_by(4)
        .map(|i| &h[i..(i + 4).min(h.len())])
        .collect();
    groups.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_well_formed() {
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), 9);
            assert_eq!(&c[4..5], "-");
            assert!(
                normalize_code(&c)
                    .bytes()
                    .all(|b| ALPHABET[..30].contains(&b))
            );
        }
        assert_eq!(normalize_code(" k7qf-3mzd "), "K7QF3MZD");
    }

    #[test]
    fn proofs_bind_code_fingerprint_and_side() {
        let fp = [1u8; 32];
        let nonce = [2u8; 16];
        let p = pc_proof("K7QF-3MZD", &fp, &nonce);
        assert_eq!(p, pc_proof("k7qf3mzd", &fp, &nonce));
        assert_ne!(p, pc_proof("K7QF-3MZE", &fp, &nonce));
        assert_ne!(p, pc_proof("K7QF-3MZD", &[3u8; 32], &nonce));
        assert_ne!(p, pc_proof("K7QF-3MZD", &fp, &[0u8; 16]));
        assert_ne!(p, agent_proof("K7QF-3MZD", &fp, &nonce));
    }

    #[test]
    fn hex_round_trip() {
        let fp = sha256(b"x");
        assert_eq!(unhex::<32>(&hex(&fp)), Some(fp));
        assert_eq!(unhex::<32>(&show_fingerprint(&fp)), Some(fp));
        assert_eq!(unhex::<32>("abc"), None);
        assert!(same(&fp, &fp) && !same(&fp, &[0; 32]));
    }
}
