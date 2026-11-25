//! Cryptographic primitives of the TapDrop protocol (PROTOCOL.md → "Pairing", "Session").
//!
//! P-256 ECDH, HKDF-SHA256 and a six-digit code derived from the shared secret, exactly as the
//! Swift host and the Kotlin client compute them, so an unmodified TapDrop phone can pair here.

use hkdf::Hkdf;
use p256::ecdh::diffie_hellman;
use p256::pkcs8::{DecodePublicKey, EncodePublicKey};
use p256::{PublicKey, SecretKey};
use sha2::{Digest, Sha256};

/// A freshly generated host key pair, with the public half in the SPKI DER form the phone expects.
pub struct HostKeyPair {
    secret: SecretKey,
}

impl HostKeyPair {
    pub fn generate() -> Self {
        Self {
            secret: SecretKey::random(&mut rand::rngs::OsRng),
        }
    }

    pub fn public_der(&self) -> Vec<u8> {
        self.secret
            .public_key()
            .to_public_key_der()
            .expect("SPKI encoding cannot fail")
            .as_bytes()
            .to_vec()
    }

    /// The raw ECDH shared secret (the x coordinate), which is what the JCA `KeyAgreement` returns.
    pub fn shared_secret(&self, peer_der: &[u8]) -> anyhow::Result<[u8; 32]> {
        let peer = PublicKey::from_public_key_der(peer_der)?;
        let shared = diffie_hellman(self.secret.to_nonzero_scalar(), peer.as_affine());
        Ok(*shared.raw_secret_bytes().as_ref())
    }
}

/// What both sides derive from one pairing handshake: the code the person compares, and the
/// long-term key every later session is built on.
pub struct PairingSecret {
    pub code: String,
    pub key: [u8; 32],
}

impl PairingSecret {
    pub fn derive(shared: &[u8; 32], phone_pub: &[u8], host_pub: &[u8]) -> Self {
        let salt = sha256(&[phone_pub, host_pub]);
        let sas = hkdf_expand(shared, &salt, "tapdrop/sas/v1", 4);
        let n = u32::from_be_bytes([sas[0], sas[1], sas[2], sas[3]]);
        let mut key = [0u8; 32];
        key.copy_from_slice(&hkdf_expand(shared, &salt, "tapdrop/pair/v1", 32));
        Self {
            code: format!("{:06}", n % 1_000_000),
            key,
        }
    }
}

/// The per-connection key: HKDF over the long-term key, salted with both nonces.
pub fn session_key(pair_key: &[u8; 32], phone_nonce: &[u8], host_nonce: &[u8]) -> [u8; 32] {
    let salt = [phone_nonce, host_nonce].concat();
    let mut key = [0u8; 32];
    key.copy_from_slice(&hkdf_expand(pair_key, &salt, "tapdrop/session/v1", 32));
    key
}

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut md = Sha256::new();
    for p in parts {
        md.update(p);
    }
    md.finalize().into()
}

fn hkdf_expand(ikm: &[u8], salt: &[u8], info: &str, len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = vec![0u8; len];
    hk.expand(info.as_bytes(), &mut out)
        .expect("HKDF output length is legal");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 5869, test case 1.
    #[test]
    fn hkdf_matches_rfc5869() {
        let ikm = vec![0x0b; 22];
        let salt: Vec<u8> = (0..13).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut out = [0u8; 42];
        hk.expand(&info, &mut out).unwrap();
        assert_eq!(
            hex(&out),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn code_is_six_digits() {
        let a = HostKeyPair::generate();
        let b = HostKeyPair::generate();
        let shared = a.shared_secret(&b.public_der()).unwrap();
        let other = b.shared_secret(&a.public_der()).unwrap();
        assert_eq!(shared, other, "both sides derive the same ECDH secret");

        let s1 = PairingSecret::derive(&shared, &a.public_der(), &b.public_der());
        let s2 = PairingSecret::derive(&other, &a.public_der(), &b.public_der());
        assert_eq!(s1.code.len(), 6);
        assert_eq!(s1.code, s2.code);
        assert_eq!(s1.key, s2.key);
    }

    #[test]
    fn session_key_depends_on_both_nonces() {
        let key = [7u8; 32];
        let a = session_key(&key, &[1; 16], &[2; 16]);
        let b = session_key(&key, &[2; 16], &[1; 16]);
        assert_ne!(a, b, "the nonce order matters");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
