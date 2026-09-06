//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Shared AEAD core for the `encryption` middleware and the at-rest encryption
//! of the file / object_store endpoints.
//!
//! `seal` produces a self-describing envelope so `open` needs no out-of-band
//! agreement beyond the keys:
//!
//! ```text
//! [version:u8=1][cipher:u8][key_id_len:u8][key_id][nonce][ciphertext‖tag]
//! ```
//!
//! Nonces are `random prefix ‖ counter`, not fully random: a random 96-bit
//! nonce would cap AES-256-GCM at the ~2^32 messages of NIST SP 800-38D, which
//! a busy route reaches in under an hour. The counter makes nonces unique for
//! the life of a `Crypto`; it starts at a random offset rather than zero, so
//! two instances (or restarts) drawing the same prefix -- 4 bytes under
//! AES-GCM -- must also overlap in counter range to collide. The nonce is
//! written into the envelope verbatim, so this is not a wire-format change.
//!
//! A non-empty `aad` is additionally prefixed with the cleartext envelope header,
//! so the cipher id, key id and nonce are covered by the tag too. Those envelopes
//! carry version 2; an empty `aad` binds nothing and stays byte-identical to v1.

// TODO key hygiene (deferred): no zeroize, so key bytes outlive use in EncryptionConfig.key,
// decode_key's Vec, Crypto.key and decrypt_keys. EncryptionConfig also derives Debug with the
// key as a plain String -- SecretExtractor covers only the serialize path.

use crate::models::{CipherKind, EncryptionConfig};
use aes_gcm::Aes256Gcm;
use anyhow::{anyhow, Context};
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, AeadInOut, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::crypto_envelope::{
    AES_GCM_NONCE_LEN, CIPHER_AES_GCM, CIPHER_XCHACHA, ENVELOPE_VERSION,
    ENVELOPE_VERSION_AAD_BOUND, XCHACHA_NONCE_LEN,
};

/// Authentication tag length; 16 bytes for both ciphers.
const TAG_LEN: usize = 16;

/// Trailing counter bytes of every nonce; the rest is the per-instance random prefix.
const NONCE_COUNTER_LEN: usize = 8;

/// The AEAD instance for one key, built once. AES-256's key schedule and GHASH
/// table cost more to set up than sealing a small payload, so they are not
/// rebuilt per message. XChaCha needs no key schedule but shares the shape.
enum Cipher {
    Xchacha(XChaCha20Poly1305),
    /// Boxed: the AES round keys and GHASH table are ~1 KiB, XChaCha's key is 32 bytes.
    Aes(Box<Aes256Gcm>),
}

impl Cipher {
    fn new(kind: CipherKind, key: &[u8; 32]) -> Self {
        match kind {
            CipherKind::Xchacha20poly1305 => Cipher::Xchacha(XChaCha20Poly1305::new(key.into())),
            CipherKind::Aes256gcm => Cipher::Aes(Box::new(Aes256Gcm::new(key.into()))),
        }
    }

    /// Encrypts `out[body..]` in place and appends the tag, so the ciphertext is
    /// written straight into the envelope rather than allocated and copied in.
    fn seal_in_place(
        &self,
        nonce: &[u8],
        aad: &[u8],
        out: &mut Vec<u8>,
        body: usize,
    ) -> anyhow::Result<()> {
        let failed = || anyhow!("AEAD encryption failed");
        let tag = match self {
            Cipher::Xchacha(cipher) => {
                let nonce: &[u8; XCHACHA_NONCE_LEN] = nonce.try_into().map_err(|_| failed())?;
                cipher.encrypt_inout_detached(nonce.into(), aad, (&mut out[body..]).into())
            }
            Cipher::Aes(cipher) => {
                let nonce: &[u8; AES_GCM_NONCE_LEN] = nonce.try_into().map_err(|_| failed())?;
                cipher.encrypt_inout_detached(nonce.into(), aad, (&mut out[body..]).into())
            }
        }
        .map_err(|_| failed())?;
        out.extend_from_slice(&tag);
        Ok(())
    }

    fn open(&self, nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
        let failed = || anyhow!("AEAD decryption failed (tampered data or wrong key)");
        let payload = Payload {
            msg: ciphertext,
            aad,
        };
        match self {
            Cipher::Xchacha(cipher) => {
                let nonce: &[u8; XCHACHA_NONCE_LEN] = nonce.try_into().map_err(|_| failed())?;
                cipher.decrypt(nonce.into(), payload)
            }
            Cipher::Aes(cipher) => {
                let nonce: &[u8; AES_GCM_NONCE_LEN] = nonce.try_into().map_err(|_| failed())?;
                cipher.decrypt(nonce.into(), payload)
            }
        }
        .map_err(|_| failed())
    }
}

/// A ready-to-use AEAD engine built from an [`EncryptionConfig`]: the active
/// seal key plus any extra decrypt-only keys (rotation).
pub struct Crypto {
    cipher: CipherKind,
    key_id: String,
    key: [u8; 32],
    active: Cipher,
    decrypt_keys: HashMap<String, [u8; 32]>,
    /// Drawn once; the leading `nonce_len - NONCE_COUNTER_LEN` bytes are used.
    nonce_prefix: [u8; XCHACHA_NONCE_LEN - NONCE_COUNTER_LEN],
    nonce_counter: AtomicU64,
    authenticate_metadata: Vec<String>,
}

/// Decodes a configured key: optional `${env:VAR}` indirection, then base64 to
/// exactly 32 bytes.
fn decode_key(configured: &str, key_id: &str) -> anyhow::Result<[u8; 32]> {
    let raw = match configured
        .strip_prefix("${env:")
        .and_then(|r| r.strip_suffix('}'))
    {
        Some(var) => std::env::var(var).with_context(|| {
            format!("environment variable '{var}' for encryption key '{key_id}' is not set")
        })?,
        None => configured.to_string(),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.trim())
        .with_context(|| format!("encryption key '{key_id}' is not valid base64"))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
        anyhow!(
            "encryption key '{key_id}' must be 32 bytes, got {}",
            bytes.len()
        )
    })
}

impl Crypto {
    pub fn new(config: &EncryptionConfig) -> anyhow::Result<Self> {
        if config.key_id.is_empty() || config.key_id.len() > u8::MAX as usize {
            return Err(anyhow!(
                "encryption key_id must be 1..=255 bytes, got {}",
                config.key_id.len()
            ));
        }
        let key = decode_key(&config.key, &config.key_id)?;
        let mut decrypt_keys = HashMap::new();
        for (id, k) in &config.decrypt_keys {
            decrypt_keys.insert(id.clone(), decode_key(k, id)?);
        }
        Ok(Self {
            cipher: config.cipher,
            key_id: config.key_id.clone(),
            active: Cipher::new(config.cipher, &key),
            key,
            decrypt_keys,
            nonce_prefix: rand::random(),
            nonce_counter: AtomicU64::new(rand::random()),
            authenticate_metadata: config.authenticate_metadata.clone(),
        })
    }

    /// At-rest sealing has no per-message metadata, so `authenticate_metadata` would be
    /// silently ignored there. Reject it instead of pretending the metadata is protected.
    pub fn new_at_rest(config: &EncryptionConfig) -> anyhow::Result<Self> {
        if !config.authenticate_metadata.is_empty() {
            return Err(anyhow!(
                "authenticate_metadata is supported by the encryption middleware only, \
                 not by at-rest file/object_store encryption"
            ));
        }
        Self::new(config)
    }

    /// Encodes the configured metadata keys into an AAD blob, in config order.
    ///
    /// Each entry is `len(key):u32 ‖ key ‖ present:u8 [‖ len(value):u32 ‖ value]`, so an
    /// absent key cannot be confused with an empty one and values cannot be shifted
    /// between keys. Returns an empty `Vec` when nothing is configured, which keeps
    /// `seal` on its original no-AAD path.
    pub fn metadata_aad(&self, metadata: &HashMap<String, String>) -> Vec<u8> {
        let mut aad = Vec::new();
        for key in &self.authenticate_metadata {
            aad.extend_from_slice(&(key.len() as u32).to_be_bytes());
            aad.extend_from_slice(key.as_bytes());
            match metadata.get(key) {
                Some(value) => {
                    aad.push(1);
                    aad.extend_from_slice(&(value.len() as u32).to_be_bytes());
                    aad.extend_from_slice(value.as_bytes());
                }
                None => aad.push(0),
            }
        }
        aad
    }

    /// Encrypts `plaintext` with the active key into a self-describing envelope.
    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
        let (cipher_byte, nonce_len) = match self.cipher {
            CipherKind::Xchacha20poly1305 => (CIPHER_XCHACHA, XCHACHA_NONCE_LEN),
            CipherKind::Aes256gcm => (CIPHER_AES_GCM, AES_GCM_NONCE_LEN),
        };
        let mut nonce_bytes = [0u8; XCHACHA_NONCE_LEN];
        let prefix_len = nonce_len - NONCE_COUNTER_LEN;
        nonce_bytes[..prefix_len].copy_from_slice(&self.nonce_prefix[..prefix_len]);
        let counter = self.nonce_counter.fetch_add(1, Ordering::Relaxed);
        nonce_bytes[prefix_len..nonce_len].copy_from_slice(&counter.to_be_bytes());
        let nonce = &nonce_bytes[..nonce_len];

        let mut out =
            Vec::with_capacity(3 + self.key_id.len() + nonce_len + plaintext.len() + TAG_LEN);
        out.push(if aad.is_empty() {
            ENVELOPE_VERSION
        } else {
            ENVELOPE_VERSION_AAD_BOUND
        });
        out.push(cipher_byte);
        out.push(self.key_id.len() as u8);
        out.extend_from_slice(self.key_id.as_bytes());
        out.extend_from_slice(nonce);
        let body = out.len();
        let bound;
        let aad = if aad.is_empty() {
            aad
        } else {
            bound = [&out[..body], aad].concat();
            &bound
        };
        out.extend_from_slice(plaintext);

        self.active.seal_in_place(nonce, aad, &mut out, body)?;
        Ok(out)
    }

    /// Parses an envelope, selects the key by its `key_id`, and decrypts.
    /// Any parse, unknown-key, or authentication failure is a hard error.
    pub fn open(&self, envelope: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
        let err = || anyhow!("invalid encryption envelope");
        let (&version, rest) = envelope.split_first().ok_or_else(err)?;
        if version != ENVELOPE_VERSION && version != ENVELOPE_VERSION_AAD_BOUND {
            return Err(anyhow!("unsupported encryption envelope version {version}"));
        }
        let (&cipher_byte, rest) = rest.split_first().ok_or_else(err)?;
        let (&key_id_len, rest) = rest.split_first().ok_or_else(err)?;
        if rest.len() < key_id_len as usize {
            return Err(err());
        }
        let (key_id, rest) = rest.split_at(key_id_len as usize);
        let key_id = std::str::from_utf8(key_id).map_err(|_| err())?;
        let (kind, nonce_len) = match cipher_byte {
            CIPHER_XCHACHA => (CipherKind::Xchacha20poly1305, XCHACHA_NONCE_LEN),
            CIPHER_AES_GCM => (CipherKind::Aes256gcm, AES_GCM_NONCE_LEN),
            other => return Err(anyhow!("unknown encryption cipher id {other}")),
        };
        if rest.len() < nonce_len {
            return Err(err());
        }
        let (nonce, ciphertext) = rest.split_at(nonce_len);
        // v1 authenticates only what the caller passed; v2 also covers the cleartext
        // header, so the version selects the construction rather than the AAD's shape.
        let bound;
        let aad = if version == ENVELOPE_VERSION {
            aad
        } else {
            bound = [&envelope[..envelope.len() - ciphertext.len()], aad].concat();
            &bound
        };

        // Anything sealed by this config opens with the pre-built cipher; only a
        // rotation key or a foreign cipher needs one built here.
        let rotated;
        let cipher = if kind == self.cipher && key_id == self.key_id {
            &self.active
        } else {
            let key = if key_id == self.key_id {
                &self.key
            } else {
                self.decrypt_keys
                    .get(key_id)
                    .ok_or_else(|| anyhow!("no decryption key for key_id '{key_id}'"))?
            };
            rotated = Cipher::new(kind, key);
            &rotated
        };
        cipher.open(nonce, aad, ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(cipher: CipherKind) -> EncryptionConfig {
        EncryptionConfig {
            cipher,
            key_id: "k1".to_string(),
            key: base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
            decrypt_keys: HashMap::new(),
            authenticate_metadata: Vec::new(),
        }
    }

    #[test]
    fn seal_open_round_trip_both_ciphers() {
        for cipher in [CipherKind::Xchacha20poly1305, CipherKind::Aes256gcm] {
            let crypto = Crypto::new(&config(cipher)).unwrap();
            let envelope = crypto.seal(b"secret payload", b"aad").unwrap();
            assert_ne!(&envelope, b"secret payload");
            assert_eq!(crypto.open(&envelope, b"aad").unwrap(), b"secret payload");
        }
    }

    /// Wire-format pin: envelopes sealed by the previous implementation (cipher built
    /// per call, ciphertext allocated and copied in) must still open unchanged.
    #[test]
    fn opens_envelopes_from_the_previous_seal() {
        const VECTORS: [(CipherKind, &str); 2] = [
            (
                CipherKind::Xchacha20poly1305,
                "0100026b31090909090909090909090909090909090909090909090909cd1781f819de5912956b645309178325d684b4b084007ae1f95d5e8bbdfa",
            ),
            (
                CipherKind::Aes256gcm,
                "0101026b3109090909090909090909090954e0e7e6db84e111c11ba757878ea2b887e0ce8ffe2dd4d315dcc7c6ae6d",
            ),
        ];
        for (cipher, hex) in VECTORS {
            let envelope: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            let crypto = Crypto::new(&config(cipher)).unwrap();
            assert_eq!(crypto.open(&envelope, b"aad").unwrap(), b"secret payload");
        }
    }

    /// Counter nonces must never repeat within one `Crypto`, which is what removes
    /// the random-nonce message budget of AES-GCM.
    #[test]
    fn nonces_are_unique_across_many_seals() {
        for cipher in [CipherKind::Xchacha20poly1305, CipherKind::Aes256gcm] {
            let nonce_len = match cipher {
                CipherKind::Xchacha20poly1305 => XCHACHA_NONCE_LEN,
                CipherKind::Aes256gcm => AES_GCM_NONCE_LEN,
            };
            let crypto = Crypto::new(&config(cipher)).unwrap();
            let nonces: std::collections::HashSet<Vec<u8>> = (0..100_000)
                .map(|_| {
                    let envelope = crypto.seal(b"x", b"").unwrap();
                    envelope[5..5 + nonce_len].to_vec()
                })
                .collect();
            assert_eq!(nonces.len(), 100_000);
        }
    }

    #[test]
    fn envelope_header_is_self_describing() {
        let crypto = Crypto::new(&config(CipherKind::Aes256gcm)).unwrap();
        let envelope = crypto.seal(b"x", b"").unwrap();
        assert_eq!(envelope[0], ENVELOPE_VERSION);
        assert_eq!(envelope[1], CIPHER_AES_GCM);
        assert_eq!(envelope[2], 2);
        assert_eq!(&envelope[3..5], b"k1");
    }

    #[test]
    fn bit_flip_and_wrong_key_fail() {
        let crypto = Crypto::new(&config(CipherKind::Xchacha20poly1305)).unwrap();
        let mut envelope = crypto.seal(b"secret", b"").unwrap();
        *envelope.last_mut().unwrap() ^= 1;
        assert!(crypto.open(&envelope, b"").is_err());

        let mut other_cfg = config(CipherKind::Xchacha20poly1305);
        other_cfg.key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let other = Crypto::new(&other_cfg).unwrap();
        let envelope = crypto.seal(b"secret", b"").unwrap();
        assert!(other.open(&envelope, b"").is_err());
    }

    #[test]
    fn rotation_key_is_used_for_unknown_active_id() {
        let old = Crypto::new(&config(CipherKind::Xchacha20poly1305)).unwrap();
        let envelope = old.seal(b"rotated", b"").unwrap();

        let mut new_cfg = config(CipherKind::Xchacha20poly1305);
        new_cfg.key_id = "k2".to_string();
        new_cfg.key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        new_cfg
            .decrypt_keys
            .insert("k1".to_string(), config(CipherKind::Xchacha20poly1305).key);
        let new = Crypto::new(&new_cfg).unwrap();
        assert_eq!(new.open(&envelope, b"").unwrap(), b"rotated");
    }

    #[test]
    fn authenticated_metadata_is_bound_to_the_ciphertext() {
        let mut cfg = config(CipherKind::Xchacha20poly1305);
        cfg.authenticate_metadata = vec!["tenant".to_string(), "kind".to_string()];
        let crypto = Crypto::new(&cfg).unwrap();

        let meta: HashMap<String, String> =
            [("tenant", "acme"), ("kind", "order"), ("trace", "t1")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let envelope = crypto
            .seal(b"payload", &crypto.metadata_aad(&meta))
            .unwrap();
        assert_eq!(
            crypto.open(&envelope, &crypto.metadata_aad(&meta)).unwrap(),
            b"payload"
        );

        let mut uncovered = meta.clone();
        uncovered.insert("trace".to_string(), "t2".to_string());
        assert!(crypto
            .open(&envelope, &crypto.metadata_aad(&uncovered))
            .is_ok());

        for tamper in [
            |m: &mut HashMap<String, String>| {
                m.insert("tenant".to_string(), "evil".to_string());
            },
            |m: &mut HashMap<String, String>| {
                m.remove("kind");
            },
            // "acme"+"order" must not re-associate as "acmeorder"+"".
            |m: &mut HashMap<String, String>| {
                m.insert("tenant".to_string(), "acmeorder".to_string());
                m.insert("kind".to_string(), String::new());
            },
        ] {
            let mut tampered = meta.clone();
            tamper(&mut tampered);
            assert!(crypto
                .open(&envelope, &crypto.metadata_aad(&tampered))
                .is_err());
        }
    }

    /// At-rest encryption has no metadata to bind, so the option must be rejected
    /// rather than quietly ignored.
    #[test]
    fn at_rest_rejects_authenticated_metadata() {
        let mut cfg = config(CipherKind::Xchacha20poly1305);
        assert!(Crypto::new_at_rest(&cfg).is_ok());
        cfg.authenticate_metadata = vec!["tenant".to_string()];
        assert!(Crypto::new_at_rest(&cfg).is_err());
    }

    #[test]
    fn rejects_bad_keys() {
        let mut cfg = config(CipherKind::Xchacha20poly1305);
        cfg.key = "not base64!!".to_string();
        assert!(Crypto::new(&cfg).is_err());
        let mut cfg = config(CipherKind::Xchacha20poly1305);
        cfg.key = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(Crypto::new(&cfg).is_err());
    }
}

/// AEAD properties. Round-tripping is the obvious one; the tamper and wrong-AAD cases are the
/// reason this is AEAD rather than a plain cipher, and neither is covered by an example test.
#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn ciphers() -> impl Strategy<Value = CipherKind> {
        prop_oneof![
            Just(CipherKind::Xchacha20poly1305),
            Just(CipherKind::Aes256gcm),
        ]
    }

    fn crypto(cipher: CipherKind) -> Crypto {
        Crypto::new(&EncryptionConfig {
            cipher,
            key_id: "k1".to_string(),
            key: base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
            decrypt_keys: HashMap::new(),
            authenticate_metadata: Vec::new(),
        })
        .unwrap()
    }

    proptest! {
        #[test]
        fn a_sealed_payload_opens_back_to_itself(
            cipher in ciphers(),
            plaintext in prop::collection::vec(any::<u8>(), 0..2048),
            aad in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            let c = crypto(cipher);
            let envelope = c.seal(&plaintext, &aad).unwrap();
            prop_assert_eq!(c.open(&envelope, &aad).unwrap(), plaintext);
        }

        /// A fresh nonce per seal means two seals of the same input never collide.
        #[test]
        fn sealing_twice_never_produces_the_same_envelope(
            cipher in ciphers(),
            plaintext in prop::collection::vec(any::<u8>(), 1..256),
        ) {
            let c = crypto(cipher);
            prop_assert_ne!(
                c.seal(&plaintext, b"").unwrap(),
                c.seal(&plaintext, b"").unwrap()
            );
        }

        #[test]
        fn flipping_any_byte_of_the_envelope_fails_to_open(
            cipher in ciphers(),
            plaintext in prop::collection::vec(any::<u8>(), 1..256),
            index in any::<prop::sample::Index>(),
            xor in 1u8..=255,
        ) {
            let c = crypto(cipher);
            let mut envelope = c.seal(&plaintext, b"").unwrap();
            let at = index.index(envelope.len());
            envelope[at] ^= xor;
            prop_assert!(c.open(&envelope, b"").is_err());
        }

        #[test]
        fn opening_with_different_aad_fails(
            cipher in ciphers(),
            plaintext in prop::collection::vec(any::<u8>(), 1..256),
            sealed_aad in prop::collection::vec(any::<u8>(), 0..32),
            opened_aad in prop::collection::vec(any::<u8>(), 0..32),
        ) {
            prop_assume!(sealed_aad != opened_aad);
            let c = crypto(cipher);
            let envelope = c.seal(&plaintext, &sealed_aad).unwrap();
            prop_assert!(c.open(&envelope, &opened_aad).is_err());
        }
    }
}
