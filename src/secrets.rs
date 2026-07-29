//! The master password store.
//!
//! When an encrypted app is set to `password_source = master`, its VeraCrypt
//! container password is kept here instead of being typed at every launch. The
//! store holds one password per app and is itself encrypted with a single
//! *master password*.
//!
//! ## Construction
//!
//! Deliberately boring and standard — the interesting cryptography is
//! VeraCrypt's, and this file only needs to protect a small text blob:
//!
//! * **Argon2id** (OWASP-recommended parameters) stretches the master password
//!   into a 256-bit key, using a random 16-byte salt stored in the file.
//! * **AES-256-GCM** encrypts the TOML payload under that key with a random
//!   96-bit nonce. GCM is authenticated, so a wrong master password, a corrupted
//!   file or a tampered one all fail loudly at decryption instead of yielding
//!   garbage.
//!
//! ## "Type it once per boot"
//!
//! Running Argon2id on every app launch would be slow and would mean typing the
//! master password constantly. Instead the *derived key* (never the master
//! password, never the app passwords) is cached in `$XDG_RUNTIME_DIR`, which is
//! a tmpfs the kernel discards on reboot. So the master password is needed once
//! per boot; after that any launch can open the store without prompting, and a
//! reboot transparently requires it again.
//!
//! The cache records the salt it was derived from, so changing the master
//! password automatically invalidates it rather than leaving a stale key that
//! decrypts nothing.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use zeroize::Zeroizing;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};

use crate::manifest::wryayer_root;

/// File magic, so a wrong file is diagnosed clearly instead of as "bad password".
const MAGIC: &[u8; 8] = b"WRYAYRPW";
const FORMAT_VERSION: u8 = 1;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Argon2id cost parameters for new stores: 19 MiB, 2 passes, 1 lane — the
/// OWASP baseline. Stored in the header so these can be raised later without
/// stranding existing files.
const M_COST: u32 = 19 * 1024;
const T_COST: u32 = 2;
const P_COST: u32 = 1;

/// Path of the encrypted password store.
pub fn store_path() -> Result<PathBuf> {
    Ok(wryayer_root()?.join(".passwords.vault"))
}

/// Path of the per-boot derived-key cache.
///
/// `$XDG_RUNTIME_DIR` is a user-private tmpfs (mode 0700, wiped at logout and
/// never written to disk), which is exactly the lifetime wanted here.
pub fn cache_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })),
    };
    Ok(dir.join("wryayer").join("master.key"))
}

/// Whether a master password store has been set up.
pub fn exists() -> bool {
    store_path().map(|p| p.exists()).unwrap_or(false)
}

/// Whether the derived key is currently cached (i.e. the master password has
/// already been entered since boot).
pub fn is_unlocked() -> bool {
    cache_path().map(|p| p.exists()).unwrap_or(false)
}

/// Cryptographically strong random bytes from the kernel CSPRNG.
fn random_bytes(n: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom").context("failed to open /dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).context("failed to read /dev/urandom")?;
    Ok(buf)
}

/// Header of the store file — everything needed to re-derive the key.
struct Header {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: Vec<u8>,
    nonce: Vec<u8>,
}

/// Serialised layout: magic ‖ version ‖ costs ‖ salt ‖ nonce ‖ ciphertext.
fn encode(header: &Header, ciphertext: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&header.m_cost.to_le_bytes());
    out.extend_from_slice(&header.t_cost.to_le_bytes());
    out.extend_from_slice(&header.p_cost.to_le_bytes());
    out.extend_from_slice(&header.salt);
    out.extend_from_slice(&header.nonce);
    out.extend_from_slice(ciphertext);
    out
}

fn decode(raw: &[u8]) -> Result<(Header, &[u8])> {
    // Magic first: a file that isn't a store at all should say so, rather than
    // being reported as a truncated one just because it happens to be short.
    if raw.len() < MAGIC.len() || &raw[..MAGIC.len()] != MAGIC {
        bail!("not a wryayer password store (bad magic)");
    }
    let fixed = MAGIC.len() + 1 + 12 + SALT_LEN + NONCE_LEN;
    if raw.len() < fixed {
        bail!("password store is truncated ({} bytes)", raw.len());
    }
    let mut p = MAGIC.len();
    let version = raw[p];
    p += 1;
    if version != FORMAT_VERSION {
        bail!("unsupported password store version {version} (this build understands {FORMAT_VERSION})");
    }
    let u32_at = |p: &mut usize| {
        let v = u32::from_le_bytes([raw[*p], raw[*p + 1], raw[*p + 2], raw[*p + 3]]);
        *p += 4;
        v
    };
    let m_cost = u32_at(&mut p);
    let t_cost = u32_at(&mut p);
    let p_cost = u32_at(&mut p);
    let salt = raw[p..p + SALT_LEN].to_vec();
    p += SALT_LEN;
    let nonce = raw[p..p + NONCE_LEN].to_vec();
    p += NONCE_LEN;
    Ok((
        Header { m_cost, t_cost, p_cost, salt, nonce },
        &raw[p..],
    ))
}

/// Stretch a master password into the 256-bit store key.
fn derive_key(password: &str, header: &Header) -> Result<Zeroizing<Vec<u8>>> {
    use argon2::{Algorithm, Argon2, Params, Version};

    let params = Params::new(header.m_cost, header.t_cost, header.p_cost, Some(KEY_LEN))
        .map_err(|e| anyhow::anyhow!("invalid Argon2 parameters: {e}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new(vec![0u8; KEY_LEN]);
    argon
        .hash_password_into(password.as_bytes(), &header.salt, &mut key)
        .map_err(|e| anyhow::anyhow!("failed to derive key from master password: {e}"))?;
    Ok(key)
}

/// The decrypted password store: one container password per app.
pub struct Store {
    /// The key the store was opened with, so `save` can re-encrypt without
    /// re-running Argon2id.
    key: Zeroizing<Vec<u8>>,
    salt: Vec<u8>,
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    entries: BTreeMap<String, String>,
}

/// Redacting `Debug`: the derived implementation would print every container
/// password verbatim into any log or panic message that formats a `Store`.
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("apps", &self.entries.keys().collect::<Vec<_>>())
            .field("key", &"<redacted>")
            .finish()
    }
}

impl Store {
    /// The password stored for `app_name`, if any.
    pub fn get(&self, app_name: &str) -> Option<&str> {
        self.entries.get(app_name).map(|s| s.as_str())
    }

    /// Add or replace the password for `app_name`.
    pub fn set(&mut self, app_name: &str, password: &str) {
        self.entries.insert(app_name.to_string(), password.to_string());
    }

    /// Forget `app_name`'s password. Returns whether an entry was removed.
    pub fn remove(&mut self, app_name: &str) -> bool {
        self.entries.remove(app_name).is_some()
    }

    /// Names of every app with a stored password.
    pub fn apps(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Re-encrypt and write the store back to disk.
    pub fn save(&self) -> Result<()> {
        // A fresh nonce every save: reusing one under the same key would be a
        // catastrophic GCM failure.
        let nonce = random_bytes(NONCE_LEN)?;
        let header = Header {
            m_cost: self.m_cost,
            t_cost: self.t_cost,
            p_cost: self.p_cost,
            salt: self.salt.clone(),
            nonce,
        };
        let plaintext = Zeroizing::new(serialize_entries(&self.entries));
        let ciphertext = encrypt(&self.key, &header.nonce, plaintext.as_bytes())?;
        write_store(&encode(&header, &ciphertext))
    }
}

fn serialize_entries(entries: &BTreeMap<String, String>) -> String {
    let mut s = String::from("# wryayer container passwords\n[passwords]\n");
    for (app, pw) in entries {
        // TOML basic strings need backslashes and quotes escaped; the generated
        // alphabet excludes both, but a hand-typed password may contain them.
        let escaped = pw.replace('\\', "\\\\").replace('"', "\\\"");
        s.push_str(&format!("{app} = \"{escaped}\"\n"));
    }
    s
}

fn parse_entries(text: &str) -> Result<BTreeMap<String, String>> {
    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(default)]
        passwords: BTreeMap<String, String>,
    }
    let doc: Doc = toml::from_str(text).context("failed to parse decrypted password store")?;
    Ok(doc.passwords)
}

fn encrypt(key: &[u8], nonce: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|_| anyhow::anyhow!("failed to encrypt the password store"))
}

fn decrypt(key: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map(Zeroizing::new)
        // GCM cannot distinguish a wrong key from a damaged file, so the
        // message has to cover both.
        .map_err(|_| {
            anyhow::anyhow!("wrong master password (or the password store has been corrupted)")
        })
}

/// Write the store atomically with owner-only permissions.
fn write_store(bytes: &[u8]) -> Result<()> {
    let path = store_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("vault.tmp");
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to install {}", path.display()))?;
    Ok(())
}

/// Create an empty store protected by `master_password`.
pub fn init(master_password: &str) -> Result<()> {
    if exists() {
        bail!(
            "a master password store already exists at {}",
            store_path()?.display()
        );
    }
    if master_password.is_empty() {
        bail!("the master password must not be empty");
    }
    let header = Header {
        m_cost: M_COST,
        t_cost: T_COST,
        p_cost: P_COST,
        salt: random_bytes(SALT_LEN)?,
        nonce: random_bytes(NONCE_LEN)?,
    };
    let key = derive_key(master_password, &header)?;
    let plaintext = Zeroizing::new(serialize_entries(&BTreeMap::new()));
    let ciphertext = encrypt(&key, &header.nonce, plaintext.as_bytes())?;
    write_store(&encode(&header, &ciphertext))?;
    cache_key(&key, &header.salt)?;
    Ok(())
}

/// Open the store with an explicit master password, caching the derived key so
/// later calls this boot can use [`open_cached`].
pub fn open(master_password: &str) -> Result<Store> {
    let path = store_path()?;
    let raw = std::fs::read(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let (header, ciphertext) = decode(&raw)?;
    let key = derive_key(master_password, &header)?;
    let plaintext = decrypt(&key, &header.nonce, ciphertext)?;
    let text = String::from_utf8(plaintext.to_vec())
        .context("decrypted password store is not valid UTF-8")?;
    let entries = parse_entries(&text)?;
    cache_key(&key, &header.salt)?;
    Ok(Store {
        key,
        salt: header.salt,
        m_cost: header.m_cost,
        t_cost: header.t_cost,
        p_cost: header.p_cost,
        entries,
    })
}

/// Open the store using this boot's cached key, without prompting.
///
/// Returns `Ok(None)` when there is no usable cache — either the master
/// password has not been entered since boot, or it was changed and the cached
/// key no longer matches the store's salt.
pub fn open_cached() -> Result<Option<Store>> {
    let path = store_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let Some((key, cached_salt)) = load_cached_key()? else {
        return Ok(None);
    };
    let raw = std::fs::read(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let (header, ciphertext) = decode(&raw)?;
    // The store was re-keyed since this cache entry was written.
    if cached_salt != header.salt {
        let _ = std::fs::remove_file(cache_path()?);
        return Ok(None);
    }
    let plaintext = decrypt(&key, &header.nonce, ciphertext)?;
    let text = String::from_utf8(plaintext.to_vec())
        .context("decrypted password store is not valid UTF-8")?;
    let entries = parse_entries(&text)?;
    Ok(Some(Store {
        key,
        salt: header.salt,
        m_cost: header.m_cost,
        t_cost: header.t_cost,
        p_cost: header.p_cost,
        entries,
    }))
}

/// Replace the master password, re-deriving the key and re-encrypting in place.
pub fn change_master(old_password: &str, new_password: &str) -> Result<()> {
    if new_password.is_empty() {
        bail!("the new master password must not be empty");
    }
    let store = open(old_password)?;
    // A new salt as well as a new key, so the old cached key is invalidated and
    // the two versions of the file share no derivation material.
    let header = Header {
        m_cost: M_COST,
        t_cost: T_COST,
        p_cost: P_COST,
        salt: random_bytes(SALT_LEN)?,
        nonce: random_bytes(NONCE_LEN)?,
    };
    let key = derive_key(new_password, &header)?;
    let plaintext = Zeroizing::new(serialize_entries(&store.entries));
    let ciphertext = encrypt(&key, &header.nonce, plaintext.as_bytes())?;
    write_store(&encode(&header, &ciphertext))?;
    cache_key(&key, &header.salt)?;
    Ok(())
}

/// Delete the store and this boot's cached key.
///
/// Deliberately does *not* need the master password: the case this exists for
/// is not knowing it. That is no weakening — anyone who can run this can
/// already `rm` the file — but it does mean the caller is responsible for
/// warning about what the store still holds.
pub fn destroy() -> Result<()> {
    let path = store_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    lock()
}

/// Forget this boot's cached key, so the master password is required again.
pub fn lock() -> Result<()> {
    let path = cache_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

/// Persist the derived key for the rest of this boot (mode 0600 on tmpfs).
fn cache_key(key: &[u8], salt: &[u8]) -> Result<()> {
    let path = cache_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {}", parent.display()))?;
    }
    let mut blob = Vec::with_capacity(salt.len() + key.len());
    blob.extend_from_slice(salt);
    blob.extend_from_slice(key);

    // Create with 0600 from the start rather than chmod-ing afterwards, so the
    // key is never briefly world-readable.
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    f.write_all(&blob)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// A cached key together with the salt it was derived from.
type CachedKey = (Zeroizing<Vec<u8>>, Vec<u8>);

/// Read this boot's cached key as `(key, salt)`.
fn load_cached_key() -> Result<Option<CachedKey>> {
    let path = cache_path()?;
    let Ok(blob) = std::fs::read(&path) else {
        return Ok(None);
    };
    if blob.len() != SALT_LEN + KEY_LEN {
        // Truncated or from an older layout — drop it and re-prompt.
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    let salt = blob[..SALT_LEN].to_vec();
    let key = Zeroizing::new(blob[SALT_LEN..].to_vec());
    Ok(Some((key, salt)))
}

// ── Terminal password entry ───────────────────────────────────────────────────

/// Prompt for a password with echo disabled.
///
/// Fails with a clear message when there is no terminal, which is what happens
/// if a caller forgets to suspend the TUI before asking.
pub fn prompt_password(prompt: &str) -> Result<Zeroizing<String>> {
    let pw = rpassword::prompt_password(prompt).context(
        "failed to read a password from the terminal — this needs an interactive terminal",
    )?;
    Ok(Zeroizing::new(pw))
}

/// Prompt twice and require the two entries to match.
pub fn prompt_new_password(prompt: &str) -> Result<Zeroizing<String>> {
    let first = prompt_password(prompt)?;
    if first.is_empty() {
        bail!("password must not be empty");
    }
    let second = prompt_password("Repeat to confirm: ")?;
    if *first != *second {
        bail!("passwords did not match");
    }
    Ok(first)
}

/// Open the store, prompting for the master password only if this boot's key
/// isn't cached yet.
pub fn open_interactive() -> Result<Store> {
    if let Some(store) = open_cached()? {
        return Ok(store);
    }
    if !exists() {
        bail!(
            "no master password store yet — create one with:\n    wryayer master init"
        );
    }
    let pw = prompt_password("Master password: ")?;
    open(&pw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point HOME and XDG_RUNTIME_DIR at a scratch dir so the tests never touch
    /// the real store.
    ///
    /// Serialised on the crate-wide lock, not a private one: this module used
    /// to hold its own, which let another module restore the real HOME between
    /// the set_var here and the write inside `f` — and that is exactly how a
    /// real user's password store came to be overwritten with a fixture.
    fn with_temp_env<T>(f: impl FnOnce() -> T) -> T {
        crate::test_support::with_temp_home(|_| f())
    }

    #[test]
    fn round_trips_passwords_through_the_store() {
        with_temp_env(|| {
            init("master-pw").unwrap();
            let mut s = open("master-pw").unwrap();
            s.set("firefox", "container-secret");
            s.set("vivaldi", "another-secret");
            s.save().unwrap();

            let s2 = open("master-pw").unwrap();
            assert_eq!(s2.get("firefox"), Some("container-secret"));
            assert_eq!(s2.get("vivaldi"), Some("another-secret"));
            assert_eq!(s2.get("absent"), None);
        });
    }

    #[test]
    fn wrong_master_password_is_rejected() {
        with_temp_env(|| {
            init("correct-horse").unwrap();
            let err = open("wrong-password").unwrap_err().to_string();
            assert!(err.contains("wrong master password"), "got: {err}");
        });
    }

    #[test]
    fn tampering_with_the_ciphertext_is_detected() {
        with_temp_env(|| {
            init("master-pw").unwrap();
            let mut s = open("master-pw").unwrap();
            s.set("app", "secret");
            s.save().unwrap();

            // Flip a bit in the last byte (inside the GCM tag / ciphertext).
            let path = store_path().unwrap();
            let mut raw = std::fs::read(&path).unwrap();
            let last = raw.len() - 1;
            raw[last] ^= 0x01;
            std::fs::write(&path, &raw).unwrap();

            assert!(open("master-pw").is_err(), "tampered store must not open");
        });
    }

    #[test]
    fn removing_an_entry_persists() {
        with_temp_env(|| {
            init("m").unwrap();
            let mut s = open("m").unwrap();
            s.set("a", "1");
            s.set("b", "2");
            s.save().unwrap();

            let mut s = open("m").unwrap();
            assert!(s.remove("a"));
            assert!(!s.remove("nope"));
            s.save().unwrap();

            let s = open("m").unwrap();
            assert_eq!(s.get("a"), None);
            assert_eq!(s.get("b"), Some("2"));
            assert_eq!(s.apps(), vec!["b".to_string()]);
        });
    }

    #[test]
    fn changing_the_master_password_preserves_entries() {
        with_temp_env(|| {
            init("old-pw").unwrap();
            let mut s = open("old-pw").unwrap();
            s.set("app", "container-pw");
            s.save().unwrap();

            change_master("old-pw", "new-pw").unwrap();

            assert!(open("old-pw").is_err(), "old password must stop working");
            let s = open("new-pw").unwrap();
            assert_eq!(s.get("app"), Some("container-pw"));
        });
    }

    #[test]
    fn change_master_rejects_a_wrong_old_password() {
        with_temp_env(|| {
            init("real").unwrap();
            assert!(change_master("bogus", "new").is_err());
            // The store must still open with the original password.
            assert!(open("real").is_ok());
        });
    }

    #[test]
    fn cached_key_opens_the_store_without_the_password() {
        with_temp_env(|| {
            init("master-pw").unwrap();
            let mut s = open("master-pw").unwrap();
            s.set("app", "pw");
            s.save().unwrap();

            assert!(is_unlocked(), "init/open should have cached the key");
            let cached = open_cached().unwrap().expect("cache should open the store");
            assert_eq!(cached.get("app"), Some("pw"));
        });
    }

    #[test]
    fn locking_clears_the_cache() {
        with_temp_env(|| {
            init("master-pw").unwrap();
            assert!(is_unlocked());
            lock().unwrap();
            assert!(!is_unlocked());
            assert!(open_cached().unwrap().is_none());
            // Locking twice is not an error.
            lock().unwrap();
        });
    }

    #[test]
    fn changing_the_master_password_invalidates_a_stale_cache() {
        with_temp_env(|| {
            init("old-pw").unwrap();
            let cache = cache_path().unwrap();
            let stale = std::fs::read(&cache).unwrap();

            change_master("old-pw", "new-pw").unwrap();

            // Restore the pre-change cache: its salt no longer matches the store.
            std::fs::write(&cache, &stale).unwrap();
            assert!(
                open_cached().unwrap().is_none(),
                "a cache from before the re-key must be refused"
            );
            assert!(!cache.exists(), "the stale cache should be deleted");
        });
    }

    #[test]
    fn open_cached_returns_none_when_no_store_exists() {
        with_temp_env(|| {
            assert!(open_cached().unwrap().is_none());
        });
    }

    #[test]
    fn init_refuses_to_overwrite_an_existing_store() {
        with_temp_env(|| {
            init("first").unwrap();
            assert!(init("second").is_err());
            // The original store is untouched.
            assert!(open("first").is_ok());
        });
    }

    #[test]
    fn init_rejects_an_empty_master_password() {
        with_temp_env(|| {
            assert!(init("").is_err());
        });
    }

    #[test]
    fn store_file_is_owner_only() {
        with_temp_env(|| {
            init("m").unwrap();
            let md = std::fs::metadata(store_path().unwrap()).unwrap();
            assert_eq!(md.permissions().mode() & 0o777, 0o600);
            let md = std::fs::metadata(cache_path().unwrap()).unwrap();
            assert_eq!(md.permissions().mode() & 0o777, 0o600);
        });
    }

    #[test]
    fn passwords_with_quotes_and_backslashes_survive() {
        with_temp_env(|| {
            init("m").unwrap();
            let mut s = open("m").unwrap();
            let nasty = r#"a"b\c"d\\"#;
            s.set("app", nasty);
            s.save().unwrap();
            assert_eq!(open("m").unwrap().get("app"), Some(nasty));
        });
    }

    #[test]
    fn a_foreign_file_is_reported_as_such() {
        with_temp_env(|| {
            let path = store_path().unwrap();
            std::fs::write(&path, b"definitely not a vault file at all").unwrap();
            let err = open("m").unwrap_err().to_string();
            assert!(err.contains("bad magic"), "got: {err}");
        });
    }

    #[test]
    fn each_save_uses_a_fresh_nonce() {
        with_temp_env(|| {
            init("m").unwrap();
            let s = open("m").unwrap();
            s.save().unwrap();
            let first = std::fs::read(store_path().unwrap()).unwrap();
            s.save().unwrap();
            let second = std::fs::read(store_path().unwrap()).unwrap();
            assert_ne!(
                first, second,
                "identical content must still re-encrypt under a new nonce"
            );
        });
    }
}
