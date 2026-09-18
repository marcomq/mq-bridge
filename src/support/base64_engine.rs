//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Standard-alphabet base64, SIMD-accelerated where the target supports it.
//!
//! `base64::engine::general_purpose::STANDARD` is the *scalar* engine. The
//! accelerated path is the separate `engine::simd::Simd` type, which detects
//! AVX2/NEON at construction time and so cannot be a `const`.

use base64::DecodeError;
use base64::Engine as _;

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod imp {
    use super::*;
    use base64::engine::general_purpose::GeneralPurposeConfig;
    use base64::engine::simd::Simd;
    use std::sync::LazyLock;

    static ENGINE: LazyLock<Simd> = LazyLock::new(|| Simd::standard(GeneralPurposeConfig::new()));

    pub fn encode(input: &[u8]) -> String {
        ENGINE.encode(input)
    }

    pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
        ENGINE.decode(input)
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    pub fn encode(input: &[u8]) -> String {
        STANDARD.encode(input)
    }

    pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
        STANDARD.decode(input)
    }
}

/// Encode `input` as padded standard-alphabet base64.
#[inline]
pub fn encode(input: &[u8]) -> String {
    imp::encode(input)
}

/// Decode padded standard-alphabet base64.
#[inline]
pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
    imp::decode(input)
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trips_binary() {
        // Long enough to exercise the SIMD block path, not just the scalar tail.
        let data: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
        let encoded = super::encode(&data);
        assert_eq!(super::decode(&encoded).unwrap(), data);
    }

    #[test]
    fn matches_reference_vectors() {
        assert_eq!(super::encode(b""), "");
        assert_eq!(super::encode(b"f"), "Zg==");
        assert_eq!(super::encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(super::decode("Zm9vYmFy").unwrap(), b"foobar");
        assert!(super::decode("!!!!").is_err());
    }

    /// Only one `imp` is compiled per target, so the reference vectors above never compare
    /// the two engines. Pin the active one against the scalar `STANDARD` engine so a
    /// padding or decode-mode difference in `Simd::standard` cannot ship unnoticed.
    #[test]
    fn agrees_with_the_scalar_engine() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;

        // Lengths 0..=3 cover every padding case; 1024 exercises the SIMD block path.
        for len in [0usize, 1, 2, 3, 1024] {
            let data: Vec<u8> = (0..=255u8).cycle().take(len).collect();
            let encoded = super::encode(&data);
            assert_eq!(
                encoded,
                STANDARD.encode(&data),
                "encode differs at len {len}"
            );
            assert_eq!(super::decode(&encoded).unwrap(), data);
        }
        // Padding must be required, not merely tolerated, in both engines.
        assert!(super::decode("Zg").is_err());
        assert!(STANDARD.decode("Zg").is_err());
    }
}
