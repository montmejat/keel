//! `keel image`: a machine's whole software, as one file a kernel boots.
//!
//! The file is an initramfs: a cpio archive the kernel unpacks into memory
//! before running its `/init`. Ours holds `keel` (static, so it needs
//! nothing else) as `/init`, the kernel modules asked for with what they
//! depend on, and this machine's token, so the daemon it starts only obeys
//! us. See `init` for what happens at boot.
//!
//! The archive is written here rather than by `cpio`: a device node
//! (`/dev/console`, without which process 1 has no output) needs root to
//! create on disk, but not to describe in an archive. Times are zero, so the
//! same keel, modules and token give the same file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::init::{HOME, MODULES};
use crate::packaging;
use crate::sha256;
use crate::wire;

const DIR: u32 = 0o040755;
const FILE: u32 = 0o100644;
const EXECUTABLE: u32 = 0o100755;
const PRIVATE: u32 = 0o100600;
const CHAR_DEVICE: u32 = 0o020600;

/// A cpio archive in the "new ASCII" format the kernel reads.
struct Archive {
    bytes: Vec<u8>,
    inode: u32,
}

impl Archive {
    fn entry(&mut self, name: &str, mode: u32, device: (u32, u32), data: &[u8]) {
        self.inode += 1;
        let links = if mode & 0o170000 == DIR & 0o170000 { 2 } else { 1 };
        let fields =
            [self.inode, mode, 0, 0, links, 0, data.len() as u32, 0, 0, device.0, device.1, name.len() as u32 + 1, 0];
        self.bytes.extend_from_slice(b"070701");
        for field in fields {
            write!(self.bytes, "{field:08x}").unwrap();
        }
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.push(0);
        self.pad();
        self.bytes.extend_from_slice(data);
        self.pad();
    }

    /// Headers and data start on 4-byte boundaries.
    fn pad(&mut self) {
        self.bytes.resize(self.bytes.len().next_multiple_of(4), 0);
    }

    fn dir(&mut self, name: &str) {
        self.entry(name, DIR, (0, 0), &[]);
    }
}

/// Builds the image for this machine's architecture and kernel `release`
/// (`uname -r`), with `modules` and their dependencies, and writes it to
/// `out`. Returns its hash and the kernel to boot it with.
pub fn build(out: &Path, workspace: &Path, release: &str, modules: &[String]) -> io::Result<(String, PathBuf)> {
    if !workspace.join("crates/keel-cli").is_dir() {
        return Err(io::Error::other("run `keel image` from keel's source tree: it builds keel for the image"));
    }
    let keel = packaging::build(workspace, &packaging::host_target(), &["keel"])?.join("keel");
    let token = wire::ensure_token()?;

    let mut archive = Archive { bytes: Vec::new(), inode: 0 };
    for dir in ["dev", "proc", "sys", "run", "tmp"] {
        archive.dir(dir);
    }
    archive.entry("dev/console", CHAR_DEVICE, (5, 1), &[]);
    archive.entry("init", EXECUTABLE, (0, 0), &fs::read(&keel)?);

    let home = HOME.trim_start_matches('/');
    for dir in [home.to_owned(), format!("{home}/.config"), format!("{home}/.config/keel")] {
        archive.dir(&dir);
    }
    archive.entry(&format!("{home}/.config/keel/token"), PRIVATE, (0, 0), format!("{token}\n").as_bytes());

    let modules_dir = MODULES.trim_start_matches('/');
    archive.dir(modules_dir);
    for (n, file) in module_files(release, modules)?.iter().enumerate() {
        let name = file.file_name().unwrap_or_default().to_string_lossy();
        let name = name.split(".ko").next().unwrap_or_default();
        archive.entry(&format!("{modules_dir}/{n:02}-{name}.ko"), FILE, (0, 0), &decompressed(file)?);
    }
    archive.entry("TRAILER!!!", 0, (0, 0), &[]);

    fs::write(out, &archive.bytes)?;
    let kernel = Path::new("/lib/modules").join(release).join("vmlinuz");
    Ok((sha256::hash(&archive.bytes), kernel))
}

/// The files of `modules` and of what they depend on, each after its
/// dependencies, as `modprobe` works it out.
fn module_files(release: &str, modules: &[String]) -> io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = Vec::new();
    for module in modules {
        let out = Command::new("modprobe").args(["--show-depends", "-S", release, module]).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "no module `{module}` for kernel {release}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // `insmod <file> <parameters>`, or `builtin <name>`: nothing to load.
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut words = line.split_whitespace();
            if let (Some("insmod"), Some(file)) = (words.next(), words.next()) {
                if !files.iter().any(|f| f == Path::new(file)) {
                    files.push(file.into());
                }
            }
        }
    }
    Ok(files)
}

/// A module's bytes: distributions compress them.
fn decompressed(module: &Path) -> io::Result<Vec<u8>> {
    let tool = match module.extension().and_then(|e| e.to_str()) {
        Some("xz") => "xz",
        Some("zst") => "zstd",
        Some("gz") => "gzip",
        _ => return fs::read(module),
    };
    let out = Command::new(tool).arg("-dc").arg(module).output()?;
    match out.status.success() {
        true => Ok(out.stdout),
        false => Err(io::Error::other(format!("{tool} failed on {}", module.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_layout() {
        let mut archive = Archive { bytes: Vec::new(), inode: 0 };
        archive.dir("dev");
        archive.entry("dev/console", CHAR_DEVICE, (5, 1), &[]);
        archive.entry("init", EXECUTABLE, (0, 0), b"hello");
        let bytes = &archive.bytes;
        assert_eq!(bytes.len() % 4, 0);
        // The directory: 110 bytes of header, `dev\0`, padded to 116.
        assert_eq!(&bytes[..6], b"070701");
        assert_eq!(&bytes[14..22], b"000041ed", "mode");
        assert_eq!(&bytes[94..102], b"00000004", "name length, with its NUL");
        assert_eq!(&bytes[110..114], b"dev\0");
        // The console: character device 5, 1.
        let console = &bytes[116..];
        assert_eq!(&console[14..22], b"00002180");
        assert_eq!((&console[78..86], &console[86..94]), (&b"00000005"[..], &b"00000001"[..]));
        // The file: its size, then its data after the padded name.
        let init = &console[124..];
        assert_eq!(&init[54..62], b"00000005");
        assert_eq!(&init[110..115], b"init\0");
        assert_eq!(&init[116..121], b"hello");
    }
}
