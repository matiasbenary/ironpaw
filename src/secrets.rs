use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use argon2::Argon2;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, AeadCore, KeyInit, OsRng},
};
use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha256;

use crate::config::RuntimeLayout;

const VERSION: u32 = 1;
#[cfg(not(test))]
const KDF_MEMORY_KIB: u32 = 65_536;
#[cfg(test)]
const KDF_MEMORY_KIB: u32 = 1_024;
const KDF_ITERATIONS: u32 = 3;
const KDF_PARALLELISM: u32 = 1;
const DOMAIN: &[u8] = b"ironpaw/secrets/v1";
const PHRASE_WORDS: usize = 6;

#[derive(Clone)]
pub struct Secrets {
    store: SecretStore,
    pub channels: ChannelSecrets,
    pub tools: ToolSecrets,
}

impl Secrets {
    pub fn locked() -> Self {
        Self::new(SecretStore::locked())
    }

    pub fn unlocked(store: Arc<EncryptedSecretStore>) -> Self {
        let secrets = Self::locked();
        secrets.unlock(store);
        secrets
    }

    fn new(store: SecretStore) -> Self {
        Self {
            channels: ChannelSecrets {
                store: store.clone(),
            },
            tools: ToolSecrets {
                store: store.clone(),
            },
            store,
        }
    }

    pub fn unlock(&self, store: Arc<EncryptedSecretStore>) {
        self.store.unlock(store);
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.store.get(name)
    }

    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.store.set(name, value)
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        self.store.remove(name)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.get(name).is_some_and(|value| !value.trim().is_empty())
    }

    pub fn store(&self) -> Arc<EncryptedSecretStore> {
        self.store
            .store()
            .expect("secrets should be unlocked before accessing store")
    }

    pub fn for_tests() -> Self {
        let layout = RuntimeLayout::from_root(
            std::env::temp_dir().join(format!("ironpaw-test-secrets-{}", uuid::Uuid::new_v4())),
        );
        let store = EncryptedSecretStore::create(&layout, "alpha bravo charlie delta echo foxtrot")
            .expect("test secrets");
        Self::unlocked(store)
    }
}

#[derive(Clone)]
pub struct SecretStore {
    inner: Arc<Mutex<Option<Arc<EncryptedSecretStore>>>>,
}

impl SecretStore {
    fn locked() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("secret store lock")
            .as_ref()
            .and_then(|store| store.get(name))
    }

    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.inner
            .lock()
            .expect("secret store lock")
            .as_ref()
            .ok_or_else(|| "secret store is locked".to_string())?
            .set(name, value)
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        self.inner
            .lock()
            .expect("secret store lock")
            .as_ref()
            .ok_or_else(|| "secret store is locked".to_string())?
            .remove(name)
    }

    pub fn exists(&self, name: &str) -> bool {
        self.get(name).is_some_and(|value| !value.trim().is_empty())
    }

    pub fn unlock(&self, store: Arc<EncryptedSecretStore>) {
        *self.inner.lock().expect("secret store lock") = Some(store);
    }

    pub fn store(&self) -> Option<Arc<EncryptedSecretStore>> {
        self.inner.lock().expect("secret store lock").clone()
    }
}

#[derive(Clone)]
pub struct ChannelSecrets {
    store: SecretStore,
}

impl ChannelSecrets {
    pub fn get(&self, name: &str) -> Option<String> {
        self.store.get(name)
    }
}

impl crate::http::SecretLookup for ChannelSecrets {
    fn get_secret(&self, name: &str) -> Option<String> {
        self.get(name)
    }
}

#[derive(Clone)]
pub struct ToolSecrets {
    store: SecretStore,
}

impl ToolSecrets {
    pub fn get(&self, name: &str) -> Option<String> {
        self.store.get(name)
    }
}

impl crate::http::SecretLookup for ToolSecrets {
    fn get_secret(&self, name: &str) -> Option<String> {
        self.get(name)
    }
}

pub struct EncryptedSecretStore {
    paths: SecretPaths,
    key: [u8; 32],
    secrets: Mutex<BTreeMap<String, String>>,
}

impl EncryptedSecretStore {
    pub fn exists(layout: &RuntimeLayout) -> bool {
        SecretPaths::new(layout).ciphertext.exists()
    }

    pub fn create(layout: &RuntimeLayout, phrase: &str) -> Result<Arc<Self>, String> {
        let paths = SecretPaths::new(layout);
        let mut salt = [0u8; 16];
        OsRng.fill(&mut salt);
        let key = derive_key(phrase, &salt)?;
        let metadata = SecretMetadata::new(salt);
        let store = Arc::new(Self {
            paths,
            key,
            secrets: Mutex::new(BTreeMap::new()),
        });
        store.save_with_metadata(metadata)?;
        Ok(store)
    }

    pub fn unlock(layout: &RuntimeLayout, phrase: &str) -> Result<Arc<Self>, String> {
        let paths = SecretPaths::new(layout);
        let metadata = read_metadata(&paths.metadata)?;
        metadata.validate()?;
        let salt = decode_fixed::<16>(&metadata.salt, "salt")?;
        let key = derive_key(phrase, &salt)?;
        let ciphertext = std::fs::read(&paths.ciphertext).map_err(|err| err.to_string())?;
        let nonce = decode_fixed::<24>(&metadata.nonce, "nonce")?;
        let plaintext = decrypt(&key, &nonce, &ciphertext)
            .map_err(|_| "failed to unlock secrets: wrong phrase or corrupted store".to_string())?;
        let secrets = serde_json::from_slice(&plaintext)
            .map_err(|err| format!("failed to parse decrypted secrets: {err}"))?;
        Ok(Arc::new(Self {
            paths,
            key,
            secrets: Mutex::new(secrets),
        }))
    }

    pub fn get(&self, name: &str) -> Option<String> {
        self.secrets
            .lock()
            .expect("secrets lock")
            .get(name)
            .cloned()
    }

    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        if value.is_empty() {
            return Ok(());
        }
        self.secrets
            .lock()
            .expect("secrets lock")
            .insert(name.to_string(), value.to_string());
        self.save()
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        self.secrets.lock().expect("secrets lock").remove(name);
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        let metadata = read_metadata(&self.paths.metadata)?;
        self.save_with_metadata(metadata)
    }

    fn save_with_metadata(&self, mut metadata: SecretMetadata) -> Result<(), String> {
        if let Some(parent) = self.paths.ciphertext.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        metadata.nonce = STANDARD.encode(nonce);
        let plaintext = serde_json::to_vec(&*self.secrets.lock().expect("secrets lock"))
            .map_err(|err| err.to_string())?;
        let ciphertext = encrypt(&self.key, &nonce.into(), &plaintext)?;
        write_metadata(&self.paths.metadata, &metadata)?;
        std::fs::write(&self.paths.ciphertext, ciphertext).map_err(|err| err.to_string())
    }
}

impl std::fmt::Debug for EncryptedSecretStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedSecretStore")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct SecretPaths {
    pub ciphertext: PathBuf,
    pub metadata: PathBuf,
}

impl SecretPaths {
    fn new(layout: &RuntimeLayout) -> Self {
        let secrets_dir = layout.root().join("secrets");
        Self {
            ciphertext: secrets_dir.join("secrets.json.enc"),
            metadata: secrets_dir.join("secrets.meta.json"),
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SecretMetadata {
    version: u32,
    kdf: String,
    kdf_memory_kib: u32,
    kdf_iterations: u32,
    kdf_parallelism: u32,
    salt: String,
    cipher: String,
    nonce: String,
}

impl SecretMetadata {
    fn new(salt: [u8; 16]) -> Self {
        Self {
            version: VERSION,
            kdf: "argon2id".to_string(),
            kdf_memory_kib: KDF_MEMORY_KIB,
            kdf_iterations: KDF_ITERATIONS,
            kdf_parallelism: KDF_PARALLELISM,
            salt: STANDARD.encode(salt),
            cipher: "xchacha20poly1305".to_string(),
            nonce: String::new(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!("unsupported secrets version: {}", self.version));
        }
        if self.kdf != "argon2id"
            || self.kdf_memory_kib != KDF_MEMORY_KIB
            || self.kdf_iterations != KDF_ITERATIONS
            || self.kdf_parallelism != KDF_PARALLELISM
        {
            return Err("unsupported secrets KDF parameters".to_string());
        }
        if self.cipher != "xchacha20poly1305" {
            return Err(format!("unsupported secrets cipher: {}", self.cipher));
        }
        Ok(())
    }
}

pub fn generate_phrase() -> String {
    let words = bip39::Language::English.word_list();
    let mut rng = rand::thread_rng();
    (0..PHRASE_WORDS)
        .map(|_| words[rng.gen_range(0..words.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn phrase_words(phrase: &str) -> Vec<String> {
    phrase
        .split_whitespace()
        .map(|word| word.trim().to_ascii_lowercase())
        .filter(|word| !word.is_empty())
        .collect()
}

pub fn phrase_is_valid(phrase: &str) -> bool {
    phrase_words(phrase).len() == PHRASE_WORDS
}

fn derive_key(phrase: &str, salt: &[u8; 16]) -> Result<[u8; 32], String> {
    let normalized = phrase_words(phrase).join(" ");
    if !phrase_is_valid(&normalized) {
        return Err(format!("unlock phrase must be {PHRASE_WORDS} words"));
    }
    let mut intermediate = [0u8; 32];
    let params = argon2::Params::new(
        KDF_MEMORY_KIB,
        KDF_ITERATIONS,
        KDF_PARALLELISM,
        Some(intermediate.len()),
    )
    .map_err(|err| err.to_string())?;
    Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
        .hash_password_into(normalized.as_bytes(), salt, &mut intermediate)
        .map_err(|err| err.to_string())?;
    let hk = Hkdf::<Sha256>::new(None, &intermediate);
    let mut key = [0u8; 32];
    hk.expand(DOMAIN, &mut key).map_err(|err| err.to_string())?;
    Ok(key)
}

fn encrypt(key: &[u8; 32], nonce: &[u8; 24], plaintext: &[u8]) -> Result<Vec<u8>, String> {
    XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(nonce), plaintext)
        .map_err(|err| err.to_string())
}

fn decrypt(key: &[u8; 32], nonce: &[u8; 24], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|err| err.to_string())
}

fn read_metadata(path: &Path) -> Result<SecretMetadata, String> {
    let json = std::fs::read_to_string(path).map_err(|err| err.to_string())?;
    serde_json::from_str(&json).map_err(|err| err.to_string())
}

fn write_metadata(path: &Path, metadata: &SecretMetadata) -> Result<(), String> {
    let json = serde_json::to_string_pretty(metadata).map_err(|err| err.to_string())?;
    std::fs::write(path, format!("{json}\n")).map_err(|err| err.to_string())
}

fn decode_fixed<const N: usize>(value: &str, name: &str) -> Result<[u8; N], String> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|err| format!("invalid {name}: {err}"))?;
    bytes
        .try_into()
        .map_err(|_| format!("invalid {name}: expected {N} bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> RuntimeLayout {
        RuntimeLayout::from_root(
            std::env::temp_dir().join(format!("ironpaw-secrets-test-{}", uuid::Uuid::new_v4())),
        )
    }

    #[test]
    fn encrypted_store_round_trips_secrets() {
        let layout = layout();
        let phrase = "alpha bravo charlie delta echo foxtrot";
        let store = EncryptedSecretStore::create(&layout, phrase).unwrap();
        store.set("EMAIL_PASSWORD", "super-secret").unwrap();

        let unlocked = EncryptedSecretStore::unlock(&layout, phrase).unwrap();

        assert_eq!(
            unlocked.get("EMAIL_PASSWORD").as_deref(),
            Some("super-secret")
        );
    }

    #[test]
    fn wrong_phrase_fails_to_unlock() {
        let layout = layout();
        EncryptedSecretStore::create(&layout, "alpha bravo charlie delta echo foxtrot").unwrap();

        let err =
            match EncryptedSecretStore::unlock(&layout, "wrong bravo charlie delta echo foxtrot") {
                Ok(_) => panic!("wrong phrase unexpectedly unlocked store"),
                Err(err) => err,
            };

        assert!(err.contains("failed to unlock secrets"));
    }

    #[test]
    fn ciphertext_does_not_contain_plaintext_secret() {
        let layout = layout();
        let store = EncryptedSecretStore::create(&layout, "alpha bravo charlie delta echo foxtrot")
            .unwrap();
        store.set("NEAR_API_KEY", "near-secret-value").unwrap();

        let ciphertext = std::fs::read(layout.root().join("secrets/secrets.json.enc")).unwrap();

        assert!(!contains_bytes(&ciphertext, b"near-secret-value"));
        assert!(!contains_bytes(&ciphertext, b"NEAR_API_KEY"));
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
