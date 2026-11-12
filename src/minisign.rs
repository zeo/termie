//! minisign signature checks for release downloads: the same key that signs
//! the rot-installer manifests signs every asset the updater fetches

use ed25519_dalek::{Signature, VerifyingKey};

/// public half of the release signing key (minisign key id EBBB27F3B1FD4311)
pub const RELEASE_KEY: &str = "RWQRQ/2x8ye761U+afRxVwfgmAO6Z8kWzoLiBuZ6Lrlm1KLuokQ/PA/8";

/// a minisign signature file never needs more than this
pub const MAX_SIGNATURE_BYTES: usize = 4096;

/// checks `sig_file` (the text of a `.minisig`) over `data` with the base64
/// minisign public key `key`; only the prehashed `ED` form is accepted and
/// the trusted comment's global signature must hold too
pub fn verify(key: &str, sig_file: &str, data: &[u8]) -> Result<(), String> {
    let key = crate::term::base64_decode(key.trim().as_bytes())
        .filter(|k| k.len() == 42 && k[..2] == *b"Ed")
        .ok_or("bad signing key")?;
    let mut lines = sig_file.lines();
    let mut field = |prefix: &str| lines.next().and_then(|l| l.strip_prefix(prefix)).ok_or("malformed signature");
    field("untrusted comment:")?;
    let sig = crate::term::base64_decode(field("")?.trim().as_bytes())
        .filter(|s| s.len() == 74)
        .ok_or("malformed signature")?;
    let trusted = field("trusted comment: ")?;
    let global = crate::term::base64_decode(field("")?.trim().as_bytes())
        .and_then(|g| <[u8; 64]>::try_from(g).ok())
        .ok_or("malformed signature")?;
    if sig[..2] != *b"ED" {
        return Err("unsupported signature algorithm".into());
    }
    if sig[2..10] != key[2..10] {
        return Err("signed by a different key".into());
    }
    let vk = VerifyingKey::from_bytes(key[10..].try_into().expect("42-byte key")).map_err(|_| "bad signing key")?;
    let raw: [u8; 64] = sig[10..].try_into().expect("74-byte signature");
    vk.verify_strict(&blake2b512(data), &Signature::from_bytes(&raw))
        .map_err(|_| "signature mismatch")?;
    let mut comment = raw.to_vec();
    comment.extend_from_slice(trusted.as_bytes());
    vk.verify_strict(&comment, &Signature::from_bytes(&global))
        .map_err(|_| "signature comment mismatch".into())
}

const IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

/// rfc 7693 blake2b with a 64-byte digest and no key
fn blake2b512(data: &[u8]) -> [u8; 64] {
    let mut h = IV;
    h[0] ^= 0x0101_0040;
    let blocks = data.len().div_ceil(128).max(1);
    for (i, chunk) in data.chunks(128).chain(data.is_empty().then_some(&[][..])).enumerate() {
        let mut block = [0u8; 128];
        block[..chunk.len()].copy_from_slice(chunk);
        let m: [u64; 16] = std::array::from_fn(|j| u64::from_le_bytes(block[j * 8..j * 8 + 8].try_into().unwrap()));
        let t = (i * 128 + chunk.len()) as u128;
        let mut v = [0u64; 16];
        v[..8].copy_from_slice(&h);
        v[8..].copy_from_slice(&IV);
        v[12] ^= t as u64;
        v[13] ^= (t >> 64) as u64;
        if i + 1 == blocks {
            v[14] = !v[14];
        }
        for round in 0..12 {
            let s = &SIGMA[round % 10];
            for (k, [a, b, c, d]) in [
                [0, 4, 8, 12],
                [1, 5, 9, 13],
                [2, 6, 10, 14],
                [3, 7, 11, 15],
                [0, 5, 10, 15],
                [1, 6, 11, 12],
                [2, 7, 8, 13],
                [3, 4, 9, 14],
            ]
            .into_iter()
            .enumerate()
            {
                v[a] = v[a].wrapping_add(v[b]).wrapping_add(m[s[2 * k]]);
                v[d] = (v[d] ^ v[a]).rotate_right(32);
                v[c] = v[c].wrapping_add(v[d]);
                v[b] = (v[b] ^ v[c]).rotate_right(24);
                v[a] = v[a].wrapping_add(v[b]).wrapping_add(m[s[2 * k + 1]]);
                v[d] = (v[d] ^ v[a]).rotate_right(16);
                v[c] = v[c].wrapping_add(v[d]);
                v[b] = (v[b] ^ v[c]).rotate_right(63);
            }
        }
        for j in 0..8 {
            h[j] ^= v[j] ^ v[j + 8];
        }
    }
    let mut out = [0u8; 64];
    for (o, w) in out.chunks_mut(8).zip(h) {
        o.copy_from_slice(&w.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn blake2b_matches_the_rfc_vectors() {
        assert_eq!(
            hex(&blake2b512(b"abc")),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923"
        );
        assert_eq!(
            hex(&blake2b512(b"")),
            "786a02f742015903c6c6fd852552d272912f4740e15847618a86e217f71f5419d25e1031afee585313896444934eb04b903a685b1448b755d56f701afe9be2ce"
        );
        // exactly one block and one byte past it exercise the final-block flag
        assert_ne!(blake2b512(&[7; 128]), blake2b512(&[7; 129]));
    }

    #[test]
    fn a_real_release_signature_verifies_and_tampering_fails() {
        let data = include_bytes!("../tests/minisign/installer.json");
        let sig = include_str!("../tests/minisign/installer.json.minisig");
        assert_eq!(verify(RELEASE_KEY, sig, data), Ok(()));
        let mut bad = data.to_vec();
        bad[10] ^= 1;
        assert_eq!(verify(RELEASE_KEY, sig, &bad), Err("signature mismatch".into()));
        let forged = sig.replace("timestamp:1785048562", "timestamp:1785048563");
        assert_eq!(verify(RELEASE_KEY, &forged, data), Err("signature comment mismatch".into()));
        assert_eq!(verify(RELEASE_KEY, "untrusted comment: x\n", data), Err("malformed signature".into()));
        let other_key = "RWQAAAAAAAAAAFU+afRxVwfgmAO6Z8kWzoLiBuZ6Lrlm1KLuokQ/PA/8";
        assert_eq!(verify(other_key, sig, data), Err("signed by a different key".into()));
    }
}
