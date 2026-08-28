use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, Generate, KeyInit, Payload, consts::U12},
};
use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use config::FileFormat;
use std::sync::{OnceLock, RwLock};
use zeroize::Zeroizing;

pub const CONFIG_MASTER_KEY_ENV: &str = "MQB_CONFIG_MASTER_KEY";
const CONFIG_ENVELOPE_VERSION: u8 = 1;
const CONFIG_ENVELOPE_ALG: &str = "AES-256-GCM";
const CONFIG_ENVELOPE_AAD: &[u8] = b"mq-bridge-app:config";
const SENSITIVE_MODE_LABEL: &str = "sensitive";
const DURABLE_MODE_LABEL: &str = "durable";
const CONFIG_MASTER_KEY_MEMORY_KID: &str = "process-memory";

#[cfg(test)]
static TEST_CONFIG_MASTER_KEY_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> =
    std::sync::OnceLock::new();
static CONFIG_MASTER_KEY_MEMORY: OnceLock<RwLock<Option<String>>> = OnceLock::new();

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, Default)]
struct EncryptedEnvelope {
    v: u8,
    alg: String,
    kid: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, Default)]
struct EncryptedConfigFile {
    #[serde(default)]
    config_security: EncryptedConfigSecurity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encrypted_config: Option<EncryptedEnvelope>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize, Clone, Default)]
struct EncryptedConfigSecurity {
    #[serde(default)]
    mode: String,
}

pub fn has_config_master_key() -> bool {
    config_master_key_memory()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .is_some()
        || std::env::var(CONFIG_MASTER_KEY_ENV).is_ok()
}

#[cfg(test)]
pub(crate) fn test_config_master_key_lock() -> &'static tokio::sync::Mutex<()> {
    TEST_CONFIG_MASTER_KEY_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub fn uses_encrypted_config_mode_label(mode: &str) -> bool {
    matches!(mode.trim(), SENSITIVE_MODE_LABEL | DURABLE_MODE_LABEL)
}

pub fn config_file_format_from_path(path: &str) -> FileFormat {
    if path.ends_with(".json") {
        FileFormat::Json
    } else {
        FileFormat::Yaml
    }
}

fn config_master_key_memory() -> &'static RwLock<Option<String>> {
    CONFIG_MASTER_KEY_MEMORY.get_or_init(|| RwLock::new(None))
}

pub fn set_process_config_master_key_hex(value: String) {
    let mut guard = config_master_key_memory()
        .write()
        .unwrap_or_else(|error| error.into_inner());
    *guard = Some(value);
}

pub fn clear_process_config_master_key() {
    let mut guard = config_master_key_memory()
        .write()
        .unwrap_or_else(|error| error.into_inner());
    *guard = None;
}

fn read_config_master_key() -> Result<(Zeroizing<Vec<u8>>, &'static str), anyhow::Error> {
    let in_memory = config_master_key_memory()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let (raw, kid) = if let Some(value) = in_memory {
        (value, CONFIG_MASTER_KEY_MEMORY_KID)
    } else {
        let value = std::env::var(CONFIG_MASTER_KEY_ENV).map_err(|_| {
            anyhow::anyhow!(
                "Sensitive config requires {} to be set to a 32-byte hex key",
                CONFIG_MASTER_KEY_ENV
            )
        })?;
        (value, CONFIG_MASTER_KEY_ENV)
    };
    let bytes = hex::decode(raw.trim()).map_err(|_| {
        anyhow::anyhow!(
            "{} must contain a valid 32-byte hex key",
            CONFIG_MASTER_KEY_ENV
        )
    })?;
    if bytes.len() != 32 {
        anyhow::bail!("{} must contain exactly 32 bytes", CONFIG_MASTER_KEY_ENV);
    }
    Ok((Zeroizing::new(bytes), kid))
}

fn encrypt_config_payload(plaintext: &str) -> Result<EncryptedEnvelope, anyhow::Error> {
    let (key, kid) = read_config_master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("Failed to initialize config encryption"))?;
    // 96-bit nonce from the OS CSPRNG (best practice), not a UUID.
    let nonce = Nonce::<U12>::generate();
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: CONFIG_ENVELOPE_AAD,
            },
        )
        .map_err(|_| anyhow::anyhow!("Failed to encrypt sensitive config"))?;

    Ok(EncryptedEnvelope {
        v: CONFIG_ENVELOPE_VERSION,
        alg: CONFIG_ENVELOPE_ALG.to_string(),
        kid: kid.to_string(),
        nonce: BASE64.encode(nonce.as_slice()),
        ciphertext: BASE64.encode(ciphertext),
    })
}

fn decrypt_config_payload(envelope: &EncryptedEnvelope) -> Result<String, anyhow::Error> {
    if envelope.v != CONFIG_ENVELOPE_VERSION {
        anyhow::bail!("Unsupported encrypted config version: {}", envelope.v);
    }
    if envelope.alg != CONFIG_ENVELOPE_ALG {
        anyhow::bail!("Unsupported encrypted config algorithm: {}", envelope.alg);
    }

    let (key, _) = read_config_master_key()?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("Failed to initialize config decryption"))?;
    let nonce_bytes = BASE64
        .decode(envelope.nonce.as_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid encrypted config nonce"))?;
    let nonce = Nonce::<U12>::try_from(nonce_bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("Invalid encrypted config nonce length"))?;
    let ciphertext = BASE64
        .decode(envelope.ciphertext.as_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid encrypted config ciphertext"))?;
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext.as_ref(),
                aad: CONFIG_ENVELOPE_AAD,
            },
        )
        .map_err(|_| anyhow::anyhow!("Failed to decrypt sensitive config"))?;
    String::from_utf8(plaintext)
        .map_err(|_| anyhow::anyhow!("Sensitive config payload is not valid UTF-8"))
}

pub fn encode_sensitive_config_file(
    plaintext: &str,
    format: FileFormat,
    mode_label: &str,
) -> Result<String, anyhow::Error> {
    let envelope = EncryptedConfigFile {
        config_security: EncryptedConfigSecurity {
            mode: mode_label.trim().to_string(),
        },
        encrypted_config: Some(encrypt_config_payload(plaintext)?),
    };

    match format {
        FileFormat::Json => serde_json::to_string_pretty(&envelope).map_err(Into::into),
        _ => serde_yaml_ng::to_string(&envelope).map_err(Into::into),
    }
}

pub fn maybe_decrypt_config_source(
    content: &str,
    format: FileFormat,
) -> Result<Option<String>, anyhow::Error> {
    let envelope = match format {
        FileFormat::Json => serde_json::from_str::<EncryptedConfigFile>(content).ok(),
        _ => serde_yaml_ng::from_str::<EncryptedConfigFile>(content).ok(),
    };

    let Some(file) = envelope else {
        return Ok(None);
    };

    if !uses_encrypted_config_mode_label(&file.config_security.mode) {
        return Ok(None);
    }

    let Some(encrypted) = file.encrypted_config.as_ref() else {
        return Ok(None);
    };
    Ok(Some(decrypt_config_payload(encrypted)?))
}

pub fn read_config_security_mode_from_str(content: &str, format: FileFormat) -> Option<String> {
    let value = match format {
        FileFormat::Json => serde_json::from_str::<serde_json::Value>(content).ok()?,
        _ => serde_yaml_ng::from_str::<serde_json::Value>(content).ok()?,
    };
    value
        .get("config_security")
        .and_then(|security| security.get("mode"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, SecretStore, load_config_at_path};
    use std::{collections::HashMap, sync::Mutex};

    const TEST_MASTER_KEY_HEX: &str =
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[derive(Default)]
    struct RecordingSecretStore {
        stored: Mutex<HashMap<String, String>>,
    }

    impl SecretStore for RecordingSecretStore {
        fn store(&self, secrets: &HashMap<String, String>) -> anyhow::Result<()> {
            self.stored.lock().unwrap().extend(secrets.clone());
            Ok(())
        }
    }

    fn env_lock() -> &'static tokio::sync::Mutex<()> {
        test_config_master_key_lock()
    }

    struct TestMasterKeyGuard;

    impl TestMasterKeyGuard {
        fn install() -> Self {
            unsafe {
                std::env::set_var(CONFIG_MASTER_KEY_ENV, TEST_MASTER_KEY_HEX);
            }
            Self
        }
    }

    impl Drop for TestMasterKeyGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(CONFIG_MASTER_KEY_ENV);
            }
        }
    }

    fn with_test_master_key<T>(f: impl FnOnce() -> T) -> T {
        let _env_guard = env_lock().blocking_lock();
        let _key_guard = TestMasterKeyGuard::install();
        f()
    }

    fn with_test_process_master_key<T>(f: impl FnOnce() -> T) -> T {
        let _env_guard = env_lock().blocking_lock();
        clear_process_config_master_key();
        unsafe {
            std::env::remove_var(CONFIG_MASTER_KEY_ENV);
        }
        set_process_config_master_key_hex(TEST_MASTER_KEY_HEX.to_string());
        let result = f();
        clear_process_config_master_key();
        result
    }

    fn sample_sensitive_config() -> AppConfig {
        serde_yaml_ng::from_str(
            r#"
config_security:
  mode: sensitive
publishers:
  - name: "orders_http"
    endpoint:
      http:
        url: "https://example.test/orders"
        custom_headers:
          authorization: "Bearer token"
"#,
        )
        .unwrap()
    }

    fn save_and_load_sensitive_config() -> (String, AppConfig) {
        let config = sample_sensitive_config();
        let path = std::env::temp_dir().join("mqb-config-sensitive-roundtrip.yml");
        let secret_store = RecordingSecretStore::default();

        config
            .save_with_secret_store(path.to_str().unwrap(), &secret_store)
            .unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        let (loaded, _) = load_config_at_path(path.to_str().unwrap()).unwrap();

        let _ = std::fs::remove_file(path);
        (saved, loaded)
    }

    #[test]
    fn sensitive_file_round_trip_encrypts_and_decrypts() {
        with_test_master_key(|| {
            let plaintext = "hello: world\nconfig_security:\n  mode: sensitive\n";
            let encoded =
                encode_sensitive_config_file(plaintext, FileFormat::Yaml, SENSITIVE_MODE_LABEL)
                    .unwrap();
            assert!(encoded.contains("encrypted_config"));
            assert!(!encoded.contains("hello: world"));

            let decoded = maybe_decrypt_config_source(&encoded, FileFormat::Yaml)
                .unwrap()
                .unwrap();
            assert_eq!(decoded, plaintext);
        });
    }

    #[test]
    fn durable_file_round_trip_encrypts_and_decrypts() {
        with_test_master_key(|| {
            let plaintext = "hello: world\nconfig_security:\n  mode: durable\n";
            let encoded =
                encode_sensitive_config_file(plaintext, FileFormat::Yaml, DURABLE_MODE_LABEL)
                    .unwrap();
            let decoded = maybe_decrypt_config_source(&encoded, FileFormat::Yaml)
                .unwrap()
                .unwrap();
            assert_eq!(decoded, plaintext);
        });
    }

    #[test]
    fn process_master_key_round_trip_encrypts_and_decrypts() {
        with_test_process_master_key(|| {
            let plaintext = "hello: world\nconfig_security:\n  mode: sensitive\n";
            let encoded =
                encode_sensitive_config_file(plaintext, FileFormat::Yaml, SENSITIVE_MODE_LABEL)
                    .unwrap();

            assert!(encoded.contains("kid: process-memory"));

            let decrypted = maybe_decrypt_config_source(&encoded, FileFormat::Yaml)
                .unwrap()
                .expect("decrypted payload");
            assert_eq!(decrypted, plaintext);
        });
    }

    #[test]
    fn reads_security_mode_from_plain_or_encrypted_file_headers() {
        let plain = "config_security:\n  mode: durable\npublishers: []\n";
        assert_eq!(
            read_config_security_mode_from_str(plain, FileFormat::Yaml).as_deref(),
            Some("durable")
        );

        with_test_master_key(|| {
            let (saved, loaded) = save_and_load_sensitive_config();
            assert!(saved.contains("encrypted_config"));
            assert!(!saved.contains("Bearer token"));
            assert_eq!(
                read_config_security_mode_from_str(&saved, FileFormat::Yaml).as_deref(),
                Some("sensitive")
            );

            let headers = match &loaded.publishers[0].endpoint.endpoint_type {
                crate::mq_bridge::models::EndpointType::Http(cfg) => cfg.custom_headers.clone(),
                other => panic!("expected http publisher, got {other:?}"),
            };

            assert_eq!(
                headers.get("authorization").map(String::as_str),
                Some("Bearer token")
            );
        });
    }

    #[test]
    fn config_module_round_trips_sensitive_file_using_encryption_module() {
        with_test_master_key(|| {
            let (saved, loaded) = save_and_load_sensitive_config();
            assert!(saved.contains("encrypted_config"));
            assert!(!saved.contains("Bearer token"));

            let headers = match &loaded.publishers[0].endpoint.endpoint_type {
                crate::mq_bridge::models::EndpointType::Http(cfg) => cfg.custom_headers.clone(),
                other => panic!("expected http publisher, got {other:?}"),
            };

            assert_eq!(
                headers.get("authorization").map(String::as_str),
                Some("Bearer token")
            );
        });
    }

    #[test]
    fn encoded_sensitive_file_requires_master_key() {
        let _guard = env_lock().blocking_lock();
        unsafe {
            std::env::remove_var(CONFIG_MASTER_KEY_ENV);
        }
        let error = encode_sensitive_config_file("a: b\n", FileFormat::Yaml, SENSITIVE_MODE_LABEL)
            .unwrap_err();
        assert!(error.to_string().contains(CONFIG_MASTER_KEY_ENV));
    }
}
