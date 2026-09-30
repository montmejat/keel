//! The store: binaries named by their SHA-256, and the deployments using them.
//!
//! ```text
//! $XDG_DATA_HOME/keel/                (default ~/.local/share/keel)
//!   store/<sha256>                    a binary, read-only
//!   refs/<deployment id>              hashes a deployment uses, one per line
//!   deployments/<name>/<id>.json      deployments made from this machine
//!   deployments/<name>/current        symlink to the one `keel start` runs
//! ```
//!
//! Daemons keep a binary as long as some ref names it; `gc` removes the rest.
//! Hashes and ids arrive over the network, so both are checked to be plain
//! hex before they're used in a path.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::sha256::Sha256;

pub struct Store {
    dir: PathBuf,
}

/// `$XDG_DATA_HOME/keel`, or `~/.local/share/keel`.
pub fn default_dir() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) => PathBuf::from(dir).join("keel"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/keel"),
    }
}

pub fn check_hash(hash: &str) -> io::Result<()> {
    match hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        true => Ok(()),
        false => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a sha256: {hash:?}"))),
    }
}

pub fn check_id(id: &str) -> io::Result<()> {
    match !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
        true => Ok(()),
        false => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a deployment id: {id:?}"))),
    }
}

impl Store {
    pub fn open() -> io::Result<Self> {
        Self::at(default_dir())
    }

    pub fn at(dir: PathBuf) -> io::Result<Self> {
        for sub in ["store", "refs", "deployments"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn blob(&self, hash: &str) -> io::Result<PathBuf> {
        check_hash(hash)?;
        Ok(self.dir.join("store").join(hash))
    }

    /// Those of `hashes` this store doesn't have.
    pub fn missing(&self, hashes: &[String]) -> io::Result<Vec<String>> {
        let mut missing = Vec::new();
        for hash in hashes {
            if !self.blob(hash)?.exists() && !missing.contains(hash) {
                missing.push(hash.clone());
            }
        }
        Ok(missing)
    }

    /// Adds `len` bytes from `r`, which must hash to `hash`. Written to a
    /// temporary file then renamed, so a blob is either whole or absent.
    pub fn put(&self, hash: &str, r: impl Read, len: u64) -> io::Result<()> {
        let target = self.blob(hash)?;
        if target.exists() {
            io::copy(&mut r.take(len), &mut io::sink())?;
            return Ok(());
        }
        let temp = self.dir.join("store").join(format!(".{hash}.{}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o755).open(&temp)?;
            let mut hasher = Sha256::new();
            let (mut r, mut buf, mut left) = (r.take(len), vec![0; 1 << 16], len);
            while left > 0 {
                let n = r.read(&mut buf)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "blob ended early"));
                }
                hasher.update(&buf[..n]);
                file.write_all(&buf[..n])?;
                left -= n as u64;
            }
            file.sync_all()?;
            let got = hasher.finish();
            if got != hash {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("blob hashes to {got}, not {hash}")));
            }
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o555))?;
            fs::rename(&temp, &target)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    pub fn put_file(&self, hash: &str, path: &Path) -> io::Result<()> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        self.put(hash, file, len)
    }

    /// Records that deployment `id` uses `hashes`.
    pub fn pin(&self, id: &str, hashes: &[String]) -> io::Result<()> {
        check_id(id)?;
        hashes.iter().try_for_each(|h| check_hash(h))?;
        fs::write(self.dir.join("refs").join(id), hashes.join("\n") + "\n")
    }

    pub fn unpin(&self, id: &str) -> io::Result<()> {
        check_id(id)?;
        match fs::remove_file(self.dir.join("refs").join(id)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Removes every blob no ref names. Returns how many, and their bytes.
    pub fn gc(&self) -> io::Result<(u64, u64)> {
        let mut referenced = std::collections::HashSet::new();
        for entry in fs::read_dir(self.dir.join("refs"))?.flatten() {
            for line in fs::read_to_string(entry.path())?.lines() {
                referenced.insert(line.to_owned());
            }
        }
        let (mut blobs, mut bytes) = (0, 0);
        for entry in fs::read_dir(self.dir.join("store"))?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !referenced.contains(&name) {
                bytes += entry.metadata().map_or(0, |m| m.len());
                fs::remove_file(entry.path())?;
                blobs += 1;
            }
        }
        Ok((blobs, bytes))
    }

    /// Blobs held, and their bytes.
    pub fn usage(&self) -> (u64, u64) {
        let entries = fs::read_dir(self.dir.join("store")).into_iter().flatten().flatten();
        entries.fold((0, 0), |(n, bytes), e| (n + 1, bytes + e.metadata().map_or(0, |m| m.len())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256;

    #[test]
    fn put_pin_gc() {
        let dir = std::env::temp_dir().join(format!("keel-store-test-{}", std::process::id()));
        let store = Store::at(dir.clone()).unwrap();
        let (a, b) = (b"binary a".as_slice(), b"binary b".as_slice());
        let (ha, hb) = (sha256::hash(a), sha256::hash(b));

        assert_eq!(store.missing(&[ha.clone(), hb.clone()]).unwrap(), [ha.clone(), hb.clone()]);
        store.put(&ha, a, a.len() as u64).unwrap();
        store.put(&hb, b, b.len() as u64).unwrap();
        assert!(store.put(&ha, b, b.len() as u64).is_ok(), "already there: nothing to check");
        let wrong = sha256::hash(b"something else");
        assert!(store.put(&wrong, a, a.len() as u64).is_err(), "must match its hash");
        assert!(store.missing(std::slice::from_ref(&ha)).unwrap().is_empty());
        assert!(store.blob("../../etc/passwd").is_err());

        store.pin("aa11", std::slice::from_ref(&ha)).unwrap();
        assert_eq!(store.gc().unwrap(), (1, b.len() as u64), "b is unreferenced");
        assert!(store.blob(&ha).unwrap().exists());
        store.unpin("aa11").unwrap();
        assert_eq!(store.gc().unwrap().0, 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
