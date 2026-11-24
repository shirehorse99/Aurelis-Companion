//! The one household this Companion belongs to: its secret, kept as `household.json` (mode 0600)
//! in the data folder. Unpairing deletes it and every store folder.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "household.json";
/// Store folders under the data folder, wiped on unpair. Later stores add their folder here.
pub const STORE_DIRS: [&str; 6] = ["ai", "guide", "picture", "household", "ai_jobs", "dvr"];

#[derive(Clone)]
pub struct Household {
    pub secret: [u8; 32],
    pub fingerprint: String,
    dir: PathBuf,
}

/// Same as the television's `HouseholdSecret.fingerprint`: the first three bytes of SHA-256 of the
/// secret's raw bytes, as upper-case hex.
pub fn fingerprint(secret: &[u8; 32]) -> String {
    Sha256::digest(secret).iter().take(3).map(|b| format!("{b:02X}")).collect()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

impl Household {
    /// `None` when nothing is paired. A file that exists but cannot be read is also `None`, with the
    /// reason on stderr: the Companion then offers a new code rather than refusing to start.
    pub fn load(dir: &Path) -> Option<Household> {
        let path = dir.join(FILE_NAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!("cannot read {}: {e}; treating the Companion as unpaired", path.display());
                return None;
            }
        };
        let secret = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("secret").and_then(|s| s.as_str()).and_then(from_hex));
        match secret {
            Some(secret) => Some(Household { fingerprint: fingerprint(&secret), secret, dir: dir.to_path_buf() }),
            None => {
                eprintln!("{} is not a household file (expected a 64-character hex secret); treating the Companion as unpaired", path.display());
                None
            }
        }
    }

    /// Writes the secret (owner-only, via a temporary file so a crash leaves no half file).
    pub fn create(dir: &Path, secret: [u8; 32]) -> Result<Household, String> {
        use std::io::Write;
        let path = dir.join(FILE_NAME);
        let tmp = dir.join(format!("{FILE_NAME}.tmp"));
        // A leftover temp file (a crash, or another tool) keeps its old mode when reopened; remove it.
        match std::fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot clear {}: {e}", tmp.display())),
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let body = serde_json::json!({ "secret": to_hex(&secret) }).to_string();
        let written = opts.open(&tmp).and_then(|mut f| {
            f.write_all(body.as_bytes())?;
            f.sync_all()
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {e}", tmp.display()));
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot move {} into place: {e}", path.display()));
        }
        // Make the rename itself durable.
        #[cfg(unix)]
        if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("cannot sync {}: {e}", dir.display()));
        }
        Ok(Household { fingerprint: fingerprint(&secret), secret, dir: dir.to_path_buf() })
    }

    /// Deletes every store folder that exists, then the secret. The secret goes last so a failure
    /// leaves the household paired and the unpair can be asked again.
    pub fn wipe(&self) -> Result<(), String> {
        for name in STORE_DIRS {
            let p = self.dir.join(name);
            match std::fs::remove_dir_all(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("cannot delete {}: {e}", p.display())),
            }
        }
        let f = self.dir.join(FILE_NAME);
        match std::fs::remove_file(&f) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot delete {}: {e}", f.display())),
        }
    }
}

#[cfg(test)]
pub struct TempDir(pub PathBuf);

#[cfg(test)]
impl TempDir {
    pub fn new() -> TempDir {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!("aurelis-companion-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).expect("temp dir");
        TempDir(p)
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_matches_the_kotlin_definition() {
        // HouseholdSecret.fingerprint: SHA-256 of the raw bytes, first 3 bytes, upper-case hex.
        // Vector: sha256(bytes 0..=31) = 630dcd29..., computed with `python3 hashlib`.
        let mut s = [0u8; 32];
        for (i, b) in s.iter_mut().enumerate() {
            *b = i as u8;
        }
        assert_eq!(fingerprint(&s), "630DCD");
        assert_eq!(fingerprint(&[7u8; 32]), "4BB06F");
    }

    #[test]
    fn create_then_load_round_trips_with_owner_only_mode() {
        let d = TempDir::new();
        assert!(Household::load(&d.0).is_none());
        let h = Household::create(&d.0, [9u8; 32]).unwrap();
        let back = Household::load(&d.0).expect("loads");
        assert_eq!((back.secret, back.fingerprint.clone()), ([9u8; 32], h.fingerprint));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.0.join(FILE_NAME)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_leftover_temp_file_does_not_loosen_the_mode_and_is_not_left_behind() {
        use std::os::unix::fs::PermissionsExt;
        let d = TempDir::new();
        let tmp = d.0.join(format!("{FILE_NAME}.tmp"));
        std::fs::write(&tmp, "old").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        Household::create(&d.0, [3u8; 32]).unwrap();
        let mode = std::fs::metadata(d.0.join(FILE_NAME)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(!tmp.exists());
    }

    #[test]
    fn a_failed_write_leaves_no_temp_file() {
        let d = TempDir::new();
        // A directory where the final file goes makes the rename fail.
        std::fs::create_dir_all(d.0.join(FILE_NAME).join("x")).unwrap();
        assert!(Household::create(&d.0, [3u8; 32]).is_err());
        assert!(!d.0.join(format!("{FILE_NAME}.tmp")).exists());
    }

    #[test]
    fn a_damaged_file_loads_as_unpaired() {
        let d = TempDir::new();
        std::fs::write(d.0.join(FILE_NAME), "{\"secret\":\"zz\"}").unwrap();
        assert!(Household::load(&d.0).is_none());
        std::fs::write(d.0.join(FILE_NAME), "not json").unwrap();
        assert!(Household::load(&d.0).is_none());
    }

    #[test]
    fn wipe_removes_the_secret_and_present_stores_and_creates_none() {
        let d = TempDir::new();
        let h = Household::create(&d.0, [1u8; 32]).unwrap();
        std::fs::create_dir_all(d.0.join("ai")).unwrap();
        std::fs::write(d.0.join("ai/x"), "1").unwrap();
        std::fs::create_dir_all(d.0.join("guide")).unwrap();
        std::fs::create_dir_all(d.0.join("household")).unwrap();
        std::fs::write(d.0.join("household/bundle.sealed"), "1").unwrap();
        h.wipe().unwrap();
        assert!(Household::load(&d.0).is_none());
        for n in STORE_DIRS {
            assert!(!d.0.join(n).exists(), "{n}");
        }
        h.wipe().unwrap(); // already gone is fine
    }
}
