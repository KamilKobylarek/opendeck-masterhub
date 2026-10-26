//! The MQTT password, kept out of OpenDeck's settings file (readable by every account on the computer): in the
//! system's keychain (macOS Keychain, Windows Credential Manager, the Secret Service on Linux) through `keyring`,
//! or, where there's none (Linux without a desktop session), in a file only the user can read. The settings only
//! say where it is (`Store`).
//!
//! On macOS the keychain ties access to the program's signature: the plugin isn't signed, so after an update macOS
//! may ask once to allow it.

use std::path::PathBuf;

/// The keychain entry: service and account.
const SERVICE: &str = "opendeck-masterhub";
const ACCOUNT: &str = "mqtt";

/// Where the password is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Store {
    Keychain,
    /// The file in the plugin's data folder, user-only.
    File,
}

impl Store {
    pub fn name(self) -> &'static str {
        match self {
            Store::Keychain => "keychain",
            Store::File => "file",
        }
    }

    pub fn from_name(name: &str) -> Option<Store> {
        match name {
            "keychain" => Some(Store::Keychain),
            "file" => Some(Store::File),
            _ => None,
        }
    }
}

fn file() -> PathBuf {
    crate::data_dir().join("mqtt-password")
}

fn entry() -> Option<keyring::Entry> {
    keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| eprintln!("keychain: {e}")).ok()
}

/// Keeps the password: in the keychain when there is one, else in the user-only file. Whatever was in the other
/// place is removed. None when neither worked.
pub fn save(password: &str) -> Option<Store> {
    if let Some(entry) = entry() {
        match entry.set_password(password) {
            Ok(()) => {
                let _ = std::fs::remove_file(file());
                return Some(Store::Keychain);
            }
            Err(e) => eprintln!("keychain: {e}; the password goes into a user-only file instead"),
        }
    }
    match write_file(password) {
        Ok(()) => Some(Store::File),
        Err(e) => {
            eprintln!("MQTT password file {}: {e}", file().display());
            None
        }
    }
}

/// The password from where `store` says it is.
pub fn load(store: Store) -> Option<String> {
    match store {
        Store::Keychain => entry()?.get_password().map_err(|e| eprintln!("keychain: {e}")).ok(),
        Store::File => std::fs::read_to_string(file()).ok(),
    }
}

/// Forgets the password wherever it is.
pub fn delete() {
    if let Some(entry) = entry() {
        let _ = entry.delete_credential();
    }
    let _ = std::fs::remove_file(file());
}

fn write_file(password: &str) -> std::io::Result<()> {
    let path = file();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut f = options.open(&path)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?; // an older file keeps its mode
    std::io::Write::write_all(&mut f, password.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_names() {
        for s in [Store::Keychain, Store::File] {
            assert_eq!(Store::from_name(s.name()), Some(s));
        }
        assert_eq!(Store::from_name("plain"), None);
    }

    /// The real keychain round trip (macOS may ask to allow it): `cargo test keychain_round_trip -- --ignored`
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let store = save("test-password").unwrap();
        println!("stored in: {store:?}");
        assert_eq!(load(store).as_deref(), Some("test-password"));
        delete();
        assert_eq!(load(store), None);
    }
}
