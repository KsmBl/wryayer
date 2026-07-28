//! Moving apps into and out of VeraCrypt containers, and managing the
//! passwords that open them.
//!
//! ## Conversion is a rollback-safe swap
//!
//! Turning an installed app into an encrypted one moves its whole tree into a
//! freshly created container. The steps are ordered so that a crash, a power
//! cut or a Ctrl-C at any point leaves exactly one recoverable state:
//!
//! 1. `~/.wryayer/<app>/` is renamed aside to `~/.wryayer/.<app>.wr-plain`
//!    (atomic — either it happened or it didn't).
//! 2. The container is created and formatted.
//! 3. The app directory is recreated with the locked-state marker and the
//!    container is mounted over it.
//! 4. The tree is copied in from the staging dir.
//! 5. Only once the copy is verified is the staging dir deleted.
//!
//! So **the staging directory existing means the conversion did not finish**,
//! whatever else is on disk. [`recover_interrupted_encrypt`] therefore rolls the
//! whole thing back — discarding the half-filled container and restoring the
//! plaintext tree — rather than trying to guess how far it got. Losing a
//! not-yet-finished encryption is free; losing the app is not.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use crate::config::{read_config, write_config, AppConfig, PasswordSource};
use crate::manifest::{app_dir, read_manifest, wryayer_root};
use crate::veracrypt;

/// Staging directory used while an *encryption* is in flight.
///
/// Its presence means "the plaintext tree is here and the container is
/// incomplete", which is exactly what [`recover_interrupted_encrypt`] acts on.
fn staging_dir(app_name: &str) -> Result<PathBuf> {
    Ok(wryayer_root()?.join(format!(".{app_name}.wr-plain")))
}

/// Staging directory used while a *decryption* is in flight.
///
/// Deliberately a different path from [`staging_dir`], because it means the
/// opposite thing: the container is still authoritative and this is a partial
/// copy out of it. Sharing one name would let the encryption recovery delete a
/// good container and keep a half-finished copy.
fn decrypt_staging_dir(app_name: &str) -> Result<PathBuf> {
    Ok(wryayer_root()?.join(format!(".{app_name}.wr-decrypt")))
}

// ── Converting an app into a container ────────────────────────────────────────

/// Secrets handed in non-interactively instead of being prompted for.
///
/// The TUI collects these in its own overlays (so it can validate them before
/// starting a long install, and keep the install's log on screen) and passes
/// them to the child process on stdin.
#[derive(Default)]
pub struct SuppliedSecrets {
    pub container: Option<Zeroizing<String>>,
    pub master: Option<Zeroizing<String>>,
    pub sudo: Option<Zeroizing<String>>,
}

impl SuppliedSecrets {
    /// Read `key=value` lines from stdin until EOF.
    ///
    /// Passed on stdin rather than argv or the environment: argv is world
    /// readable through `/proc`, and an environment variable would be inherited
    /// by veracrypt and every other child process.
    pub fn from_stdin() -> Result<Self> {
        use std::io::BufRead;
        let mut out = Self::default();
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let line = line.context("failed to read secrets from stdin")?;
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = Zeroizing::new(value.to_string());
            match key {
                "container" => out.container = Some(value),
                "master" => out.master = Some(value),
                "sudo" => out.sudo = Some(value),
                _ => {}
            }
        }
        Ok(out)
    }
}

/// The container password for a pending encryption, resolved and validated.
///
/// Produced by [`prepare`] *before* any long-running work starts, so a mistyped
/// master password costs a re-prompt rather than a completed multi-gigabyte
/// install that then refuses to encrypt.
pub struct Prepared {
    password: Zeroizing<String>,
    use_master: bool,
}

/// Resolve every secret an encryption will need, up front.
///
/// Also records the container password in the master store when that's the
/// chosen source. Storing it *before* the container exists is deliberate: a
/// stored password for an app that failed to encrypt is harmless and gets
/// overwritten, whereas a container created but never recorded would be
/// permanently unopenable.
pub fn prepare(
    app_name: &str,
    use_master: bool,
    generate: bool,
    supplied: &SuppliedSecrets,
) -> Result<Prepared> {
    if !veracrypt::available() {
        return Err(veracrypt::missing_binary_error());
    }
    if let Some(sudo) = &supplied.sudo {
        veracrypt::prime_sudo(sudo)?;
    }

    let password = obtain_password(app_name, use_master, generate, supplied)?;
    Ok(Prepared { password, use_master })
}

/// Move `app_name`'s tree into a new VeraCrypt container.
///
/// `use_master` stores the container password in the master password store
/// (creating it if needed) instead of requiring it at every launch;
/// `generate` produces the password with the multi-source generator rather than
/// asking the user to type one.
pub fn run(app_name: &str, use_master: bool, generate: bool) -> Result<()> {
    let prepared = prepare(app_name, use_master, generate, &SuppliedSecrets::default())?;
    run_prepared(app_name, prepared)
}

/// Perform the conversion using secrets already resolved by [`prepare`].
pub fn run_prepared(app_name: &str, prepared: Prepared) -> Result<()> {
    let Prepared { password, use_master } = prepared;
    if !veracrypt::available() {
        return Err(veracrypt::missing_binary_error());
    }
    recover_interrupted_encrypt(app_name)?;

    if veracrypt::is_encrypted(app_name) {
        bail!("'{app_name}' is already stored in an encrypted container");
    }

    let manifest =
        read_manifest(app_name).with_context(|| format!("'{app_name}' is not installed"))?;
    if let Some(target) = &manifest.app.alias_of {
        bail!(
            "'{app_name}' is an alias whose files live in '{target}' — encrypt that instead:\n    \
             wryayer encrypt {target}"
        );
    }

    let dir = app_dir(app_name)?;
    let used = veracrypt::tree_size(&dir);
    let size = veracrypt::recommended_size(used);
    println!(
        "Encrypting '{app_name}': {} of files -> {} container",
        human_size(used),
        human_size(size)
    );

    let staging = staging_dir(app_name)?;
    let container = veracrypt::container_path(app_name)?;

    // 1. Park the plaintext tree aside. From here on, the staging dir's
    //    existence marks the conversion as incomplete.
    std::fs::rename(&dir, &staging).with_context(|| {
        format!("failed to move {} aside to {}", dir.display(), staging.display())
    })?;

    // Anything that fails from here rolls the app back to plaintext.
    let result = (|| -> Result<()> {
        // 2. Create and format the container (prompts for sudo).
        veracrypt::create(&container, size, &password)?;

        // 3. Recreate the app dir with the marker that keeps it listable while
        //    locked, then mount the container over it.
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to recreate {}", dir.display()))?;
        let mut marker = veracrypt::Marker::from_manifest(&manifest);
        marker.password_source =
            if use_master { "master".into() } else { "prompt".into() };
        veracrypt::write_marker(app_name, &marker)?;
        veracrypt::mount(app_name, &password)?;

        // 4. Copy the tree in.
        println!("Moving files into the container…");
        copy_tree(&staging, &dir, used)?;
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("error during encryption, rolling back: {e:#}");
        rollback_to_plaintext(app_name)?;
        return Err(e);
    }

    // 5. The copy succeeded — drop the plaintext original.
    std::fs::remove_dir_all(&staging)
        .with_context(|| format!("failed to remove {}", staging.display()))?;

    // config.ini lives inside the tree, so this has to happen after the copy.
    let mut config = read_config(app_name).unwrap_or_default();
    config.password_source = if use_master {
        PasswordSource::Master
    } else {
        PasswordSource::Prompt
    };
    write_config(app_name, &config)?;

    println!("'{app_name}' is now stored in {}", container.display());
    if use_master {
        println!("Its password is in the master store; unlock happens automatically.");
    } else {
        println!("You will be asked for its password on every launch.");
    }
    Ok(())
}

/// Decide the container password for a new encryption, storing it in the master
/// store when that's the chosen source.
fn obtain_password(
    app_name: &str,
    use_master: bool,
    generate: bool,
    supplied: &SuppliedSecrets,
) -> Result<Zeroizing<String>> {
    if !use_master {
        if let Some(pw) = &supplied.container {
            return Ok(pw.clone());
        }
        if generate {
            let (pw, report) = crate::entropy::generate_password(crate::entropy::DEFAULT_LENGTH)?;
            println!("Generated password (entropy: {}):\n\n    {}\n", report.summary(), pw.as_str());
            println!(
                "Write this down now — with password_source = prompt it is not stored anywhere,\n\
                 and without it the container cannot be opened."
            );
            return Ok(pw);
        }
        return crate::secrets::prompt_new_password(&format!(
            "New password for '{app_name}' container: "
        ));
    }

    // Master mode: make sure the store exists, then record the password in it.
    if !crate::secrets::exists() {
        let master = match &supplied.master {
            Some(m) => m.clone(),
            None => {
                println!("No master password store yet — creating one.");
                crate::secrets::prompt_new_password("New master password: ")?
            }
        };
        crate::secrets::init(&master)?;
    }
    let mut store = open_master_store(supplied)?;

    let password = if let Some(pw) = &supplied.container {
        pw.clone()
    } else if generate {
        let (pw, report) = crate::entropy::generate_password(crate::entropy::DEFAULT_LENGTH)?;
        println!("Generated a {}-character password (entropy: {}).", pw.chars().count(), report.summary());
        pw
    } else {
        crate::secrets::prompt_new_password(&format!("New password for '{app_name}' container: "))?
    };
    store.set(app_name, &password);
    store.save()?;
    Ok(password)
}

/// How many times an interactive master-password entry may be retried.
const MASTER_PASSWORD_ATTEMPTS: usize = 3;

/// Open the master store, using a supplied password if there is one and
/// otherwise prompting — with retries, so one typo doesn't abort the operation
/// the user was in the middle of.
fn open_master_store(supplied: &SuppliedSecrets) -> Result<crate::secrets::Store> {
    if let Some(master) = &supplied.master {
        return crate::secrets::open(master);
    }
    if let Some(store) = crate::secrets::open_cached()? {
        return Ok(store);
    }
    let mut last_err = None;
    for attempt in 1..=MASTER_PASSWORD_ATTEMPTS {
        let pw = crate::secrets::prompt_password("Master password: ")?;
        match crate::secrets::open(&pw) {
            Ok(store) => return Ok(store),
            Err(e) => {
                if attempt < MASTER_PASSWORD_ATTEMPTS {
                    eprintln!("{e:#} — try again ({} left)", MASTER_PASSWORD_ATTEMPTS - attempt);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("could not open the master password store")))
}

/// Undo a partial conversion, restoring the plaintext tree from staging.
fn rollback_to_plaintext(app_name: &str) -> Result<()> {
    let dir = app_dir(app_name)?;
    let staging = staging_dir(app_name)?;
    let container = veracrypt::container_path(app_name)?;

    if !staging.exists() {
        // Nothing was parked aside, so there is nothing to restore.
        return Ok(());
    }
    // The app dir may still be a live mount point. Deleting it while mounted
    // would recursively wipe the *container's* contents instead of the empty
    // directory, so a failed unmount has to stop the rollback rather than be
    // ignored. The plaintext tree is safe in staging either way.
    veracrypt::dismount(app_name).with_context(|| {
        format!(
            "could not unmount '{app_name}' to roll back — your files are intact in {}",
            staging.display()
        )
    })?;
    if veracrypt::is_mounted(app_name).unwrap_or(false) {
        bail!(
            "'{app_name}' is still mounted — refusing to roll back. Your files are intact in {}",
            staging.display()
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_file(&container);
    veracrypt::remove_marker(app_name);
    std::fs::rename(&staging, &dir).with_context(|| {
        format!(
            "failed to restore {} from {} — the app's files are safe there",
            dir.display(),
            staging.display()
        )
    })?;
    Ok(())
}

/// Roll back an encryption that was interrupted before it finished.
///
/// Called before any operation that touches an app's tree, mirroring
/// [`crate::commands::update::recover_interrupted_update`].
pub fn recover_interrupted_encrypt(app_name: &str) -> Result<()> {
    let staging = staging_dir(app_name)?;
    if !staging.exists() {
        return Ok(());
    }
    eprintln!("note: a previous encryption of '{app_name}' was interrupted — restoring it");
    rollback_to_plaintext(app_name)
}

// ── Converting an app back to plaintext ───────────────────────────────────────

/// Move `app_name`'s tree out of its container and delete the container.
pub fn decrypt(app_name: &str) -> Result<()> {
    if !veracrypt::is_encrypted(app_name) {
        bail!("'{app_name}' is not stored in an encrypted container");
    }
    ensure_unlocked(app_name)?;

    let dir = app_dir(app_name)?;
    let staging = decrypt_staging_dir(app_name)?;
    if staging.exists() {
        bail!(
            "leftover staging directory {} — remove it and try again",
            staging.display()
        );
    }

    // Copy out of the container first; the app dir is a mount point, so nothing
    // can simply be renamed across it.
    let used = veracrypt::tree_size(&dir);
    println!("Decrypting '{app_name}': copying {} out of the container…", human_size(used));
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create {}", staging.display()))?;
    if let Err(e) = copy_tree(&dir, &staging, used) {
        // The container is untouched and still holds everything, so the partial
        // copy is worthless — drop it rather than leave it to be mistaken for
        // real data.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    // Now the container's contents are safely outside it.
    veracrypt::dismount(app_name)?;
    let container = veracrypt::container_path(app_name)?;
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("failed to remove the old mount point {}", dir.display()))?;
    std::fs::rename(&staging, &dir).with_context(|| {
        format!("failed to move {} back to {}", staging.display(), dir.display())
    })?;
    std::fs::remove_file(&container)
        .with_context(|| format!("failed to delete {}", container.display()))?;

    // The marker was only relevant while the app could be locked.
    veracrypt::remove_marker(app_name);

    // Drop the stored password, if there was one.
    if let Ok(Some(mut store)) = crate::secrets::open_cached() {
        if store.remove(app_name) {
            store.save()?;
        }
    }

    let mut config = read_config(app_name).unwrap_or_default();
    config.password_source = PasswordSource::Prompt;
    write_config(app_name, &config)?;

    println!("'{app_name}' is no longer encrypted.");
    Ok(())
}

// ── Locking / unlocking ───────────────────────────────────────────────────────

/// Mount `app_name`'s container, asking for or looking up its password.
pub fn unlock(app_name: &str) -> Result<()> {
    if !veracrypt::is_encrypted(app_name) {
        bail!("'{app_name}' is not stored in an encrypted container");
    }
    if veracrypt::is_mounted(app_name)? {
        println!("'{app_name}' is already unlocked.");
        return Ok(());
    }
    ensure_unlocked(app_name)?;
    println!("'{app_name}' is unlocked.");
    Ok(())
}

/// Unmount `app_name`'s container so its files are inaccessible again.
pub fn lock(app_name: &str) -> Result<()> {
    if !veracrypt::is_encrypted(app_name) {
        bail!("'{app_name}' is not stored in an encrypted container");
    }
    if !veracrypt::is_mounted(app_name)? {
        println!("'{app_name}' is already locked.");
        return Ok(());
    }
    veracrypt::dismount(app_name)?;
    println!("'{app_name}' is locked.");
    Ok(())
}

/// Make sure `app_name`'s container is mounted, resolving its password from
/// whichever source its config names. No-op for unencrypted apps.
///
/// This is the single entry point used by `wryayer run`, so every launch path
/// gets the same behaviour.
pub fn ensure_unlocked(app_name: &str) -> Result<()> {
    if !veracrypt::is_encrypted(app_name) || veracrypt::is_mounted(app_name)? {
        return Ok(());
    }
    let password = resolve_password(app_name)?;
    veracrypt::mount(app_name, &password)
}

/// Look up or ask for `app_name`'s container password.
fn resolve_password(app_name: &str) -> Result<Zeroizing<String>> {
    if password_source(app_name) == PasswordSource::Master {
        if let Some(pw) = password_from_master(app_name)? {
            return Ok(pw);
        }
        bail!(
            "'{app_name}' is set to use the master password store, but the store has no \
             password for it. Add one with:\n    wryayer master set {app_name}"
        );
    }
    crate::secrets::prompt_password(&format!("Password for '{app_name}': "))
}

/// Fetch `app_name`'s password from the master store, prompting for the master
/// password only if this boot's key isn't cached yet.
fn password_from_master(app_name: &str) -> Result<Option<Zeroizing<String>>> {
    if !crate::secrets::exists() {
        return Ok(None);
    }
    let store = match crate::secrets::open_cached()? {
        Some(s) => s,
        None => {
            // First unlock since boot.
            let master = crate::secrets::prompt_password("Master password: ")?;
            crate::secrets::open(&master)?
        }
    };
    Ok(store.get(app_name).map(|p| Zeroizing::new(p.to_string())))
}

/// Refuse to operate on an app whose container is locked.
///
/// While locked the app directory is an empty mount point, so anything that
/// reads or rewrites the tree would either fail confusingly or — worse —
/// succeed against nothing and write its result into the *underlying*
/// directory, where the next mount would hide it. Every such command calls this
/// first.
pub fn require_unlocked(app_name: &str, what: &str) -> Result<()> {
    if veracrypt::is_locked(app_name) {
        bail!(
            "cannot {what} '{app_name}': it is stored in a locked encrypted container.\n\
             Unlock it first:\n    wryayer unlock {app_name}"
        );
    }
    Ok(())
}

/// Whether `app_name`'s container should be unmounted when the app exits.
///
/// True only for encrypted apps set to `password_source = prompt`, where
/// leaving the container mounted would mean the password isn't actually
/// required before the next start. Read before the app runs, because
/// afterwards the config is only reachable while still mounted.
pub fn should_relock_on_exit(app_name: &str) -> bool {
    veracrypt::is_encrypted(app_name) && password_source(app_name) == PasswordSource::Prompt
}

/// Unmount `app_name`'s container after the app has exited.
///
/// Never fails the launch: if another instance of the app is still running, the
/// kernel refuses to unmount a busy filesystem and the container simply stays
/// open — which is the correct outcome, not an error.
pub fn relock_on_exit(app_name: &str, relock: bool) {
    if !relock {
        return;
    }
    if let Err(e) = veracrypt::dismount(app_name) {
        eprintln!("note: leaving '{app_name}' unlocked: {e:#}");
    }
}

// ── Status ────────────────────────────────────────────────────────────────────

/// Print every encrypted app and whether it is currently unlocked.
pub fn status() -> Result<()> {
    let apps = crate::manifest::list_all_apps()?;
    let encrypted: Vec<_> = apps
        .iter()
        .filter(|m| veracrypt::is_encrypted(&m.app.name))
        .collect();

    if encrypted.is_empty() {
        println!("No apps are stored in encrypted containers.");
    } else {
        println!("{:<24} {:<10} {:>10}  SOURCE", "APP", "STATE", "SIZE");
        for m in encrypted {
            let name = &m.app.name;
            let state = if veracrypt::is_mounted(name)? { "unlocked" } else { "locked" };
            let size = veracrypt::container_path(name)
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|md| human_size(md.len()))
                .unwrap_or_else(|| "?".into());
            // Read through the marker, so this stays accurate while locked.
            let source = match password_source(name) {
                PasswordSource::Master => "master",
                PasswordSource::Prompt => "prompt",
            };
            println!("{name:<24} {state:<10} {size:>10}  {source}");
        }
    }

    println!();
    if crate::secrets::exists() {
        println!(
            "Master password store: {} this boot",
            if crate::secrets::is_unlocked() { "unlocked" } else { "locked" }
        );
    } else {
        println!("Master password store: not set up (wryayer master init)");
    }
    Ok(())
}

// ── Master password store commands ────────────────────────────────────────────

/// Create the master password store.
pub fn master_init() -> Result<()> {
    let master = crate::secrets::prompt_new_password("New master password: ")?;
    crate::secrets::init(&master)?;
    println!(
        "Master password store created at {}",
        crate::secrets::store_path()?.display()
    );
    println!("You'll be asked for this password once per boot.");
    Ok(())
}

/// Change the master password.
pub fn master_change() -> Result<()> {
    let old = crate::secrets::prompt_password("Current master password: ")?;
    let new = crate::secrets::prompt_new_password("New master password: ")?;
    crate::secrets::change_master(&old, &new)?;
    println!("Master password changed.");
    Ok(())
}

/// Forget this boot's cached key, so the master password is needed again.
pub fn master_lock() -> Result<()> {
    crate::secrets::lock()?;
    println!("Master password store locked — it will be needed again on next use.");
    Ok(())
}

/// List which apps have a password on file (never the passwords themselves).
pub fn master_list() -> Result<()> {
    let store = crate::secrets::open_interactive()?;
    let apps = store.apps();
    if apps.is_empty() {
        println!("No app passwords stored.");
        return Ok(());
    }
    println!("Stored container passwords:");
    for app in apps {
        println!("  {app}");
    }
    Ok(())
}

/// Set (or replace) the stored password for `app_name`.
///
/// With `generate`, a fresh password comes from the multi-source generator;
/// otherwise the user types it.
///
/// This only records the password — it does **not** re-key the container, so it
/// is for telling wryayer a password it doesn't already know. Re-keying an
/// existing container is done with VeraCrypt itself
/// (`veracrypt --text --change <container>`).
pub fn master_set(app_name: &str, generate: bool) -> Result<()> {
    let mut store = crate::secrets::open_interactive()?;
    let password = if generate {
        let (pw, report) = crate::entropy::generate_password(crate::entropy::DEFAULT_LENGTH)?;
        println!("Generated password (entropy: {}):\n\n    {}\n", report.summary(), pw.as_str());
        pw
    } else {
        crate::secrets::prompt_new_password(&format!("Password for '{app_name}': "))?
    };
    store.set(app_name, &password);
    store.save()?;
    println!("Stored a password for '{app_name}'.");
    println!(
        "note: this records the password only — it does not re-key an existing container."
    );
    Ok(())
}

/// Remove `app_name`'s stored password.
pub fn master_forget(app_name: &str) -> Result<()> {
    let mut store = crate::secrets::open_interactive()?;
    if !store.remove(app_name) {
        bail!("no stored password for '{app_name}'");
    }
    store.save()?;
    println!("Forgot the stored password for '{app_name}'.");
    Ok(())
}

/// Print a freshly generated password without storing it anywhere.
pub fn generate_password(length: usize) -> Result<()> {
    let (pw, report) = crate::entropy::generate_password(length)?;
    println!("{}", pw.as_str());
    eprintln!("entropy sources: {}", report.summary());
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Recursive copy preserving symlinks, permissions and — importantly — hard
/// links.
///
/// Snapshots and `wryayer dedup` share file content between paths via hard
/// links. A naive copy would give every link its own inode and could multiply
/// the tree's real size several times over, overflowing a container sized from
/// the deduplicated total. Tracking `(dev, ino)` and re-linking repeats keeps
/// the copy the same size as the original.
fn copy_tree(src: &Path, dst: &Path, total: u64) -> Result<()> {
    use std::collections::HashMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let mut links: HashMap<(u64, u64), PathBuf> = HashMap::new();
    let mut copied: u64 = 0;
    let mut last_report: u64 = 0;
    let mut stack: Vec<(PathBuf, PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf())];
    // Destination directory modes, applied only once the whole copy is done.
    // Setting them during the walk would be self-defeating: a source directory
    // shipped read-only (0555 is common in packages) would leave its
    // destination unwritable before its contents had been copied in.
    let mut dir_modes: Vec<(PathBuf, u32)> = Vec::new();

    while let Some((s, d)) = stack.pop() {
        std::fs::create_dir_all(&d).with_context(|| format!("mkdir {}", d.display()))?;
        if let Ok(md) = std::fs::metadata(&s) {
            dir_modes.push((d.clone(), md.mode()));
        }
        // A directory the owner can't read or search stops the walk dead.
        // Borrow the missing bits for the duration and hand them back.
        let dir_guard = ModeGuard::grant(&s, 0o500);
        let entries = std::fs::read_dir(&s).with_context(|| format!("read {}", s.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let target = d.join(entry.file_name());
            let Ok(ft) = entry.file_type() else { continue };

            if ft.is_symlink() {
                let link = std::fs::read_link(&path)
                    .with_context(|| format!("readlink {}", path.display()))?;
                let _ = std::fs::remove_file(&target);
                std::os::unix::fs::symlink(&link, &target)
                    .with_context(|| format!("symlink {}", target.display()))?;
            } else if ft.is_dir() {
                stack.push((path, target));
            } else if ft.is_file() {
                let md = entry
                    .metadata()
                    .with_context(|| format!("stat {}", path.display()))?;

                // A file with more than one link may already have been copied
                // under a different name; re-link instead of copying again.
                if md.nlink() > 1 {
                    let key = (md.dev(), md.ino());
                    if let Some(first) = links.get(&key) {
                        std::fs::hard_link(first, &target).with_context(|| {
                            format!("link {} -> {}", first.display(), target.display())
                        })?;
                        continue;
                    }
                    links.insert(key, target.clone());
                }

                // Some packages ship helpers the owner cannot read — dbus's
                // setuid dbus-daemon-launch-helper is the classic, mode 4750.
                // fs::copy has to read the source, so borrow the read bit for
                // the copy and restore the mode afterwards. Skipping the file
                // instead would silently drop it out of the app.
                let guard = ModeGuard::grant(&path, 0o400);
                let result = std::fs::copy(&path, &target);
                drop(guard);
                result.with_context(|| {
                    format!("copy {} -> {}", path.display(), target.display())
                })?;

                let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(md.mode()));
                copied += md.len();
                if total > 0 && copied - last_report >= 64 * 1024 * 1024 {
                    eprintln!("PROGRESS {copied}/{total}");
                    last_report = copied;
                }
            }
        }
        drop(dir_guard);
    }

    // Now that every file is in place, stamp the directory modes.
    for (dir, mode) in dir_modes {
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode));
    }
    if total > 0 {
        eprintln!("PROGRESS {total}/{total}");
    }
    Ok(())
}

/// Temporarily adds permission bits to a path, restoring the original mode when
/// dropped.
///
/// Package trees legitimately contain files and directories the owner cannot
/// read (setuid helpers) or search. Rather than skipping them — which would
/// quietly lose data — wryayer borrows the bits it needs for the length of one
/// operation. Restoring on drop means an error path can't leave the tree with
/// loosened permissions.
struct ModeGuard {
    path: PathBuf,
    original: u32,
}

impl ModeGuard {
    /// Grant `bits` if they aren't already set. Returns `None` when nothing
    /// needed changing, or when the mode couldn't be read or altered.
    fn grant(path: &Path, bits: u32) -> Option<Self> {
        use std::os::unix::fs::PermissionsExt;
        let original = std::fs::symlink_metadata(path).ok()?.permissions().mode();
        if original & bits == bits {
            return None;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(original | bits)).ok()?;
        Some(Self { path: path.to_path_buf(), original })
    }
}

impl Drop for ModeGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            &self.path,
            std::fs::Permissions::from_mode(self.original),
        );
    }
}

/// Format a byte count as a short human-readable string.
fn human_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1024 * 1024 * 1024, "GB"),
        (1024 * 1024, "MB"),
        (1024, "KB"),
        (1, "B"),
    ];
    for (div, unit) in UNITS {
        if bytes >= div {
            let v = bytes as f64 / div as f64;
            return if v >= 10.0 || div == 1 {
                format!("{:.0} {unit}", v)
            } else {
                format!("{:.1} {unit}", v)
            };
        }
    }
    "0 B".to_string()
}

/// Whether an app's config should offer the encryption section — i.e. it is
/// stored in a container.
pub fn is_encrypted_app(app_name: &str) -> bool {
    veracrypt::is_encrypted(app_name)
}

/// The password source in effect for `app_name`, defaulting to prompt.
///
/// The marker is authoritative, not `config.ini`: the config lives *inside* the
/// container, so while locked it can't be read — which is exactly when the
/// unlock path needs to know whether to prompt or consult the master store.
/// The config is only consulted for apps with no marker at all.
pub fn password_source(app_name: &str) -> PasswordSource {
    if let Some(marker) = veracrypt::read_marker(app_name) {
        return match marker.password_source.as_str() {
            "master" => PasswordSource::Master,
            _ => PasswordSource::Prompt,
        };
    }
    read_config(app_name)
        .map(|c: AppConfig| c.password_source)
        .unwrap_or(PasswordSource::Prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` with HOME pointed at a fresh scratch dir, so the recovery tests
    /// operate on a throwaway ~/.wryayer. Serialised because HOME is global.
    fn with_temp_home<T>(f: impl FnOnce(&Path) -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        std::fs::create_dir_all(tmp.path().join(".wryayer")).unwrap();

        let out = f(&tmp.path().join(".wryayer"));

        match old_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        out
    }

    #[test]
    fn recovery_restores_an_interrupted_encryption() {
        with_temp_home(|root| {
            // An encryption that died after parking the tree aside: staging
            // holds everything, the app dir is a bare would-be mount point.
            let staging = root.join(".demo.wr-plain");
            std::fs::create_dir_all(staging.join("usr/bin")).unwrap();
            std::fs::write(staging.join("usr/bin/demo"), b"binary").unwrap();
            std::fs::create_dir_all(root.join("demo")).unwrap();

            recover_interrupted_encrypt("demo").unwrap();

            assert!(!staging.exists(), "staging should be consumed");
            assert_eq!(
                std::fs::read(root.join("demo/usr/bin/demo")).unwrap(),
                b"binary",
                "the plaintext tree must be restored in place"
            );
        });
    }

    #[test]
    fn recovery_ignores_a_decryption_in_progress() {
        with_temp_home(|root| {
            // A decryption uses its own staging path. Treating it as an
            // interrupted *encryption* would delete the container and promote a
            // possibly-partial copy — so recovery must not touch it.
            let decrypt_staging = root.join(".demo.wr-decrypt");
            std::fs::create_dir_all(&decrypt_staging).unwrap();
            std::fs::write(decrypt_staging.join("partial"), b"half").unwrap();

            recover_interrupted_encrypt("demo").unwrap();

            assert!(
                decrypt_staging.join("partial").exists(),
                "decryption staging must be left alone by encryption recovery"
            );
        });
    }

    #[test]
    fn recovery_is_a_noop_with_nothing_staged() {
        with_temp_home(|root| {
            std::fs::create_dir_all(root.join("demo")).unwrap();
            std::fs::write(root.join("demo/.manifest.toml"), b"x").unwrap();

            recover_interrupted_encrypt("demo").unwrap();

            assert!(root.join("demo/.manifest.toml").exists());
        });
    }

    #[test]
    fn password_source_comes_from_the_marker_not_the_config() {
        with_temp_home(|root| {
            // config.ini lives inside the container and is unreadable while
            // locked, so the marker has to be the authority. Here they
            // disagree on purpose: the marker must win.
            std::fs::create_dir_all(root.join("demo")).unwrap();
            std::fs::write(
                root.join("demo/config.ini"),
                "[encryption]\npassword_source = prompt\n",
            )
            .unwrap();

            let mut marker = veracrypt::Marker {
                name: "demo".into(),
                main_binary: "demo".into(),
                installed_at: "now".into(),
                launchers: vec![],
                alias_of: None,
                display_name: None,
                pkg_name: None,
                password_source: "master".into(),
            };
            veracrypt::write_marker("demo", &marker).unwrap();
            assert_eq!(password_source("demo"), PasswordSource::Master);

            marker.password_source = "prompt".into();
            veracrypt::write_marker("demo", &marker).unwrap();
            assert_eq!(password_source("demo"), PasswordSource::Prompt);
        });
    }

    #[test]
    fn password_source_defaults_to_prompt_without_a_marker() {
        with_temp_home(|root| {
            std::fs::create_dir_all(root.join("demo")).unwrap();
            assert_eq!(password_source("demo"), PasswordSource::Prompt);
        });
    }

    #[test]
    fn a_legacy_in_app_dir_marker_is_still_read() {
        with_temp_home(|root| {
            // Containers created before the marker moved beside the container
            // must keep listing and unlocking.
            std::fs::create_dir_all(root.join("demo")).unwrap();
            std::fs::write(
                root.join("demo").join(veracrypt::MARKER_FILE),
                "name = \"demo\"\nmain_binary = \"demo\"\ninstalled_at = \"now\"\n\
                 launchers = [\"demo\"]\npassword_source = \"master\"\n",
            )
            .unwrap();
            let m = veracrypt::read_marker("demo").expect("legacy marker should be read");
            assert_eq!(m.name, "demo");
            assert_eq!(password_source("demo"), PasswordSource::Master);
        });
    }

    #[test]
    fn encrypt_and_decrypt_staging_paths_differ() {
        with_temp_home(|_| {
            assert_ne!(
                staging_dir("app").unwrap(),
                decrypt_staging_dir("app").unwrap()
            );
        });
    }

    #[test]
    fn human_size_uses_sensible_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2 * 1024), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_size(1536 * 1024 * 1024), "1.5 GB");
        // Past 10 the fraction stops being useful.
        assert_eq!(human_size(20 * 1024 * 1024 * 1024), "20 GB");
    }

    #[test]
    fn copy_tree_preserves_hard_links() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();

        std::fs::write(src.join("a.bin"), vec![7u8; 4096]).unwrap();
        // Two extra names for the same inode, one in a subdirectory.
        std::fs::hard_link(src.join("a.bin"), src.join("b.bin")).unwrap();
        std::fs::hard_link(src.join("a.bin"), src.join("sub/c.bin")).unwrap();

        copy_tree(&src, &dst, 0).unwrap();

        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(dst.join("a.bin")).unwrap();
        let b = std::fs::metadata(dst.join("b.bin")).unwrap();
        let c = std::fs::metadata(dst.join("sub/c.bin")).unwrap();
        assert_eq!(a.ino(), b.ino(), "hard link was copied as a separate file");
        assert_eq!(a.ino(), c.ino(), "hard link across dirs was not preserved");
        assert_eq!(std::fs::read(dst.join("b.bin")).unwrap(), vec![7u8; 4096]);
    }

    #[test]
    fn copy_tree_preserves_symlinks_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();

        std::fs::write(src.join("run.sh"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(src.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::os::unix::fs::symlink("run.sh", src.join("link")).unwrap();
        // A dangling symlink must survive too — app trees are full of them.
        std::os::unix::fs::symlink("/nonexistent/target", src.join("dangling")).unwrap();

        copy_tree(&src, &dst, 0).unwrap();

        let md = std::fs::symlink_metadata(dst.join("link")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(std::fs::read_link(dst.join("link")).unwrap(), Path::new("run.sh"));
        assert_eq!(
            std::fs::read_link(dst.join("dangling")).unwrap(),
            Path::new("/nonexistent/target")
        );
        let mode = std::fs::metadata(dst.join("run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "exec bit lost");
    }

    #[test]
    fn copy_tree_copies_a_file_the_owner_cannot_read() {
        // dbus ships usr/lib/dbus-1.0/dbus-daemon-launch-helper as mode 4750 —
        // no owner read bit — which made fs::copy fail and aborted the whole
        // encryption.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("usr/lib/dbus-1.0")).unwrap();
        let helper = src.join("usr/lib/dbus-1.0/dbus-daemon-launch-helper");
        std::fs::write(&helper, b"ELF-ish payload").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o4750)).unwrap();

        copy_tree(&src, &dst, 0).unwrap();

        let out = dst.join("usr/lib/dbus-1.0/dbus-daemon-launch-helper");
        // Read it back through a borrowed read bit, the same way the copy did.
        let _g = ModeGuard::grant(&out, 0o400);
        assert_eq!(std::fs::read(&out).unwrap(), b"ELF-ish payload");
        drop(_g);
        assert_eq!(
            std::fs::metadata(&out).unwrap().permissions().mode() & 0o7777,
            0o4750,
            "the setuid helper's mode must be reproduced exactly"
        );
        // The source must be left exactly as it was found.
        assert_eq!(
            std::fs::metadata(&helper).unwrap().permissions().mode() & 0o7777,
            0o4750,
            "the source mode must be restored"
        );
    }

    #[test]
    fn copy_tree_handles_read_only_directories() {
        // A 0555 directory must still receive its contents, and end up 0555.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("ro")).unwrap();
        std::fs::write(src.join("ro/data.txt"), b"payload").unwrap();
        std::fs::set_permissions(src.join("ro"), std::fs::Permissions::from_mode(0o555)).unwrap();

        copy_tree(&src, &dst, 0).unwrap();

        assert_eq!(std::fs::read(dst.join("ro/data.txt")).unwrap(), b"payload");
        assert_eq!(
            std::fs::metadata(dst.join("ro")).unwrap().permissions().mode() & 0o777,
            0o555,
            "directory mode should be reproduced"
        );
        // Make the temp dir removable again.
        std::fs::set_permissions(src.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(dst.join("ro"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn copy_tree_descends_into_unsearchable_directories() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("locked")).unwrap();
        std::fs::write(src.join("locked/secret.bin"), b"inside").unwrap();
        // No owner read/execute at all — read_dir would fail outright.
        std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();

        copy_tree(&src, &dst, 0).unwrap();

        std::fs::set_permissions(dst.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(std::fs::read(dst.join("locked/secret.bin")).unwrap(), b"inside");
        std::fs::set_permissions(src.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn copy_tree_recurses_into_nested_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("a/b/c")).unwrap();
        std::fs::write(src.join("a/b/c/deep.txt"), b"deep").unwrap();
        std::fs::write(src.join("top.txt"), b"top").unwrap();

        copy_tree(&src, &dst, 0).unwrap();

        assert_eq!(std::fs::read(dst.join("a/b/c/deep.txt")).unwrap(), b"deep");
        assert_eq!(std::fs::read(dst.join("top.txt")).unwrap(), b"top");
    }
}
