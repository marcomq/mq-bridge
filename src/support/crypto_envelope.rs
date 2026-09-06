//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Wire-format constants of the crypto envelope. Kept out of [`super::crypto`]
//! so at-rest detection still works without the `encryption` feature.

pub(crate) const ENVELOPE_VERSION: u8 = 1;
/// As v1, but the AAD is the cleartext header followed by the caller's AAD. Only
/// produced when the caller authenticates something; at-rest sealing never does.
#[cfg(feature = "encryption")]
pub(crate) const ENVELOPE_VERSION_AAD_BOUND: u8 = 2;
pub(crate) const CIPHER_XCHACHA: u8 = 0;
pub(crate) const CIPHER_AES_GCM: u8 = 1;
#[cfg(feature = "encryption")]
pub(crate) const XCHACHA_NONCE_LEN: usize = 24;
pub(crate) const AES_GCM_NONCE_LEN: usize = 12;

/// Header (`version`, `cipher`, `key_id_len`) + shortest key id + shortest nonce.
pub(crate) const MIN_ENVELOPE_LEN: usize = 3 + 1 + AES_GCM_NONCE_LEN;
