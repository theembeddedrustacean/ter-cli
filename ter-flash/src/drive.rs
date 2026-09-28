//! UF2 drives: the USB drive a board's bootloader shows, known by the
//! INFO_UF2.TXT at its root.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::uf2::{FAMILIES, Family};

pub const INFO_FILE: &str = "INFO_UF2.TXT";
/// The name ter gives the file it copies; bootloaders take any name.
pub const FILE_NAME: &str = "ter.uf2";

/// A mounted UF2 drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drive {
    pub path: PathBuf,
    /// Its INFO_UF2.TXT.
    pub info: String,
    /// The block device it is mounted from, where ter knows it (Linux).
    pub device: Option<PathBuf>,
}

impl Drive {
    /// The UF2 drive mounted at `path`, if that is one.
    pub fn at(path: &Path) -> Option<Self> {
        let info = std::fs::read_to_string(path.join(INFO_FILE)).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            info,
            device: None,
        })
    }

    /// A line of INFO_UF2.TXT: `Model`, `Board-ID`.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.info.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == name).then(|| v.trim())
        })
    }

    /// The board, for people: `Raspberry Pi RP2 (RPI-RP2)`.
    pub fn board(&self) -> String {
        match (self.field("Model"), self.field("Board-ID")) {
            (Some(m), Some(b)) if m != b => format!("{m} ({b})"),
            (Some(n), _) | (None, Some(n)) => n.to_string(),
            (None, None) => "a UF2 bootloader".into(),
        }
    }

    /// The family whose bootloader this is, when ter can tell.
    pub fn family(&self) -> Option<&'static Family> {
        FAMILIES.into_iter().find(|f| f.is_in(&self.info))
    }

    /// Whether the drive is still there. A board that restarted leaves a
    /// stale mount on some systems, so its device counts too.
    pub fn present(&self) -> bool {
        self.device.as_ref().is_none_or(|d| d.exists()) && self.path.join(INFO_FILE).is_file()
    }

    /// Copy `uf2` to the drive. The bootloader restarts the board once the
    /// last block is in, which can cut the final flush short: an error
    /// with the drive gone is not a failed copy.
    pub fn copy(&self, uf2: &[u8]) -> io::Result<()> {
        let written = std::fs::File::create(self.path.join(FILE_NAME)).and_then(|mut f| {
            f.write_all(uf2)?;
            f.sync_all()
        });
        match written {
            Err(_) if !self.present() => Ok(()),
            other => other,
        }
    }

    /// Wait up to `limit` for the drive to go; whether it went.
    pub fn wait_gone(&self, limit: Duration) -> bool {
        let started = Instant::now();
        while self.present() {
            if started.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }
}

/// Every mounted UF2 drive.
pub fn mounted() -> Vec<Drive> {
    let mut drives: Vec<Drive> = mount_points()
        .into_iter()
        .filter_map(|(path, device)| {
            Drive::at(&path).map(|mut d| {
                d.device = device;
                d
            })
        })
        .collect();
    drives.sort_by(|a, b| a.path.cmp(&b.path));
    drives
}

/// Where FAT drives are mounted, and from which device. Only FAT: a UF2
/// drive is one, and looking into a network mount can hang.
#[cfg(target_os = "linux")]
fn mount_points() -> Vec<(PathBuf, Option<PathBuf>)> {
    std::fs::read_to_string("/proc/self/mounts")
        .map(|t| parse_mounts(&t))
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn mount_points() -> Vec<(PathBuf, Option<PathBuf>)> {
    std::fs::read_dir("/Volumes")
        .map(|d| d.flatten().map(|e| (e.path(), None)).collect())
        .unwrap_or_default()
}

#[cfg(windows)]
fn mount_points() -> Vec<(PathBuf, Option<PathBuf>)> {
    (b'D'..=b'Z')
        .map(|l| (PathBuf::from(format!("{}:\\", l as char)), None))
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn mount_points() -> Vec<(PathBuf, Option<PathBuf>)> {
    Vec::new()
}

/// FAT mounts from a `/proc/self/mounts` text.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_mounts(text: &str) -> Vec<(PathBuf, Option<PathBuf>)> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(' ');
            let (source, target, kind) = (f.next()?, f.next()?, f.next()?);
            matches!(kind, "vfat" | "msdos" | "exfat" | "fuseblk").then(|| {
                let device = source.starts_with("/dev/").then(|| unescape(source).into());
                (PathBuf::from(unescape(target)), device)
            })
        })
        .collect()
}

/// A mounts field with its `\040`-style escapes undone.
fn unescape(field: &str) -> String {
    let mut out = Vec::new();
    let bytes = field.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let code = bytes
            .get(i + 1..i + 4)
            .and_then(|o| std::str::from_utf8(o).ok())
            .and_then(|o| u8::from_str_radix(o, 8).ok());
        match (bytes[i], code) {
            (b'\\', Some(c)) => {
                out.push(c);
                i += 4;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// UF2 bootloaders on USB that are not mounted, as their block devices: a
/// machine without a desktop does not mount them by itself. Linux only.
pub fn unmounted() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let names: Vec<String> = std::fs::read_dir("/dev/disk/by-id")
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let mounted: Vec<PathBuf> = mount_points()
            .into_iter()
            .filter_map(|(_, d)| d.and_then(|d| d.canonicalize().ok()))
            .collect();
        let mut found: Vec<PathBuf> = bootloader_disks(&names)
            .into_iter()
            .filter_map(|n| Path::new("/dev/disk/by-id").join(n).canonicalize().ok())
            .filter(|d| !mounted.contains(d))
            .collect();
        found.dedup();
        found
    }
    #[cfg(not(target_os = "linux"))]
    Vec::new()
}

/// Of `/dev/disk/by-id` names, the UF2 bootloaders' disks: the first
/// partition where the disk has one (RPI-RP2), else the disk (Adafruit's
/// bootloaders show a bare FAT volume).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn bootloader_disks(names: &[String]) -> Vec<&str> {
    let looks = |n: &str| n.starts_with("usb-") && (n.contains("RPI_RP2") || n.contains("UF2"));
    names
        .iter()
        .map(String::as_str)
        .filter(|n| looks(n) && !n.contains("-part"))
        .map(|disk| {
            names
                .iter()
                .map(String::as_str)
                .find(|n| n.strip_prefix(disk) == Some("-part1"))
                .unwrap_or(disk)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drive_is_known_by_its_info_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Drive::at(dir.path()), None);
        std::fs::write(
            dir.path().join(INFO_FILE),
            "UF2 Bootloader v3.0\nModel: Raspberry Pi RP2\nBoard-ID: RPI-RP2\n",
        )
        .unwrap();
        let d = Drive::at(dir.path()).unwrap();
        assert_eq!(d.board(), "Raspberry Pi RP2 (RPI-RP2)");
        assert_eq!(d.family().map(|f| f.name), Some("rp2040"));
        assert!(d.present());

        d.copy(b"blocks").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(FILE_NAME)).unwrap(),
            b"blocks"
        );
        assert!(!d.wait_gone(Duration::from_millis(100)));
        std::fs::remove_file(dir.path().join(INFO_FILE)).unwrap();
        assert!(d.wait_gone(Duration::from_millis(100)));

        let gone = Drive {
            device: Some("/dev/ter-no-such-disk".into()),
            ..Drive::at(dir.path()).unwrap_or(d)
        };
        assert!(!gone.present(), "its device went with the board");
    }

    #[test]
    fn only_fat_mounts_are_looked_into() {
        let mounts = "\
sysfs /sys sysfs rw 0 0
/dev/sda2 / ext4 rw 0 0
/dev/sdb1 /media/omar/RPI-RP2 vfat rw 0 0
/dev/sdc /mnt/XIAO\\040SENSE vfat rw 0 0
server:/share /net nfs rw 0 0
";
        assert_eq!(
            parse_mounts(mounts),
            [
                (
                    PathBuf::from("/media/omar/RPI-RP2"),
                    Some(PathBuf::from("/dev/sdb1"))
                ),
                (
                    PathBuf::from("/mnt/XIAO SENSE"),
                    Some(PathBuf::from("/dev/sdc"))
                ),
            ]
        );
    }

    #[test]
    fn bootloader_disks_are_found_by_their_usb_names() {
        let names: Vec<String> = [
            "usb-RPI_RP2_E0C9125B0D9B-0:0",
            "usb-RPI_RP2_E0C9125B0D9B-0:0-part1",
            "usb-Adafruit_nRF_UF2_9A6F0D6E-0:0",
            "usb-SanDisk_Cruzer_1234-0:0",
            "usb-SanDisk_Cruzer_1234-0:0-part1",
            "ata-Samsung_SSD",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            bootloader_disks(&names),
            [
                "usb-RPI_RP2_E0C9125B0D9B-0:0-part1",
                "usb-Adafruit_nRF_UF2_9A6F0D6E-0:0"
            ]
        );
    }
}
