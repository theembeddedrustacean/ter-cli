//! UF2: boards whose bootloader shows up as a USB drive (XIAO RP2040, XIAO
//! nRF52840). A `.uf2` file copied to the drive is written to flash by the
//! bootloader, which then restarts the board into the program.
//!
//! ter makes the file itself from the ELF cargo built, so flashing needs no
//! other tool. For the RP2040 the file is the one elf2uf2-rs makes, byte
//! for byte.

use std::collections::BTreeMap;
use std::ops::Range;

/// A chip family the UF2 bootloaders know, and what ter needs to know
/// about its boards.
#[derive(Debug, PartialEq, Eq)]
pub struct Family {
    /// The name runners use for it (`uf2deploy -f nrf52840`).
    pub name: &'static str,
    /// The family ID in every block; a bootloader ignores other families.
    pub id: u32,
    pub chip: &'static str,
    /// Where a program may put bytes: the flash the bootloader writes.
    pub flash: Range<u32>,
    /// What the learner does to get the drive, for people.
    pub bootloader: &'static str,
    /// Text in the drive's INFO_UF2.TXT that says it is this family.
    pub info: &'static [&'static str],
    /// Why a program outside `flash` is wrong on this board, for people.
    pub outside: &'static str,
    /// Fill each 4 KiB sector a program touches with empty pages: the
    /// RP2040 boot ROM erases sectors by block number.
    pad_sectors: bool,
}

pub const RP2040: Family = Family {
    name: "rp2040",
    id: 0xe48b_ff56,
    chip: "RP2040",
    flash: 0x1000_0000..0x1500_0000,
    bootloader: "hold BOOT, tap RESET, then let go of BOOT; a drive named RPI-RP2 appears",
    info: &["RPI-RP2", "Raspberry Pi RP2"],
    outside: "an RP2040 program runs from flash at 0x10000000 (memory.x)",
    pad_sectors: true,
};

/// With the stock Adafruit bootloader Seeed ships: the SoftDevice sits
/// below the program and the bootloader above it, and it refuses blocks
/// outside the gap (0x26000 is the lowest start, for S140 6.x).
pub const NRF52840: Family = Family {
    name: "nrf52840",
    id: 0xada5_2840,
    chip: "nRF52840",
    flash: 0x0002_6000..0x000F_4000,
    bootloader: "double-tap RESET; a new USB drive appears",
    info: &["nRF52840", "NRF52840"],
    outside: "this board's bootloader keeps the SoftDevice below 0x27000, so the program starts there (memory.x)",
    pad_sectors: false,
};

pub const FAMILIES: [&Family; 2] = [&RP2040, &NRF52840];

impl Family {
    /// A family by its name or its ID in hex (`0xada52840`).
    pub fn named(name: &str) -> Option<&'static Family> {
        let id = name
            .strip_prefix("0x")
            .or_else(|| name.strip_prefix("0X"))
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        FAMILIES
            .into_iter()
            .find(|f| f.name.eq_ignore_ascii_case(name) || Some(f.id) == id)
    }

    /// Whether a drive's INFO_UF2.TXT is this family's bootloader.
    pub fn is_in(&self, info: &str) -> bool {
        self.info.iter().any(|s| info.contains(s))
    }
}

const PAGE: u32 = 256;
const SECTOR: u32 = 4096;
const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;
const FLAG_FAMILY_ID: u32 = 0x0000_2000;
/// Every block is 512 bytes: a 32-byte header, 476 of data, the end magic.
pub const BLOCK: usize = 512;

/// A 256-byte page of flash and which of its bytes the program sets.
struct Page {
    data: [u8; PAGE as usize],
    set: [bool; PAGE as usize],
}

/// The UF2 file for `elf`, or why ter cannot make one.
pub fn from_elf(elf: &[u8], family: &Family) -> Result<Vec<u8>, String> {
    let segments = load_segments(elf, family)?;
    let mut pages: BTreeMap<u32, Page> = BTreeMap::new();
    for (addr, bytes) in segments {
        let end = addr as u64 + bytes.len() as u64;
        if addr < family.flash.start || end > family.flash.end as u64 {
            return Err(format!(
                "the program puts {} bytes at {addr:#010x}, outside the {}'s flash ({:#010x} to {:#010x}): {}",
                bytes.len(),
                family.chip,
                family.flash.start,
                family.flash.end,
                family.outside
            ));
        }
        for (i, b) in bytes.iter().enumerate() {
            let a = addr + i as u32;
            let page = pages.entry(a & !(PAGE - 1)).or_insert(Page {
                data: [0; PAGE as usize],
                set: [false; PAGE as usize],
            });
            let at = (a & (PAGE - 1)) as usize;
            if page.set[at] {
                return Err(format!("two parts of the program overlap at {a:#010x}"));
            }
            page.set[at] = true;
            page.data[at] = *b;
        }
    }
    let Some(&last) = pages.keys().next_back() else {
        return Err("the program has nothing to write to flash".into());
    };
    if family.pad_sectors {
        let sectors: Vec<u32> = pages.keys().map(|a| a / SECTOR).collect();
        for sector in sectors {
            for page in (sector * SECTOR..(sector + 1) * SECTOR).step_by(PAGE as usize) {
                if page < last {
                    pages.entry(page).or_insert(Page {
                        data: [0; PAGE as usize],
                        set: [false; PAGE as usize],
                    });
                }
            }
        }
    }

    let count = pages.len() as u32;
    let mut out = Vec::with_capacity(pages.len() * BLOCK);
    for (n, (addr, page)) in pages.into_iter().enumerate() {
        for word in [
            MAGIC_START0,
            MAGIC_START1,
            FLAG_FAMILY_ID,
            addr,
            PAGE,
            n as u32,
            count,
            family.id,
        ] {
            out.extend_from_slice(&word.to_le_bytes());
        }
        out.extend_from_slice(&page.data);
        out.resize(out.len() + 476 - PAGE as usize, 0);
        out.extend_from_slice(&MAGIC_END.to_le_bytes());
    }
    Ok(out)
}

/// The bytes each loadable segment puts in memory, at its load address.
fn load_segments<'a>(elf: &'a [u8], family: &Family) -> Result<Vec<(u32, &'a [u8])>, String> {
    let not_for = |why: &str| format!("the program is not one for the {}: {why}", family.chip);
    if elf.get(..4) != Some(b"\x7fELF") {
        return Err(not_for("it is not an ELF file"));
    }
    if elf.get(4) != Some(&1) || elf.get(5) != Some(&1) {
        return Err(not_for("it is not a 32-bit little-endian ELF"));
    }
    let u16_at = |at: usize| {
        elf.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or_else(|| "the ELF file is cut short".to_string())
    };
    let u32_at = |at: usize| {
        elf.get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| "the ELF file is cut short".to_string())
    };
    const EM_ARM: u16 = 40;
    if u16_at(0x12)? != EM_ARM {
        return Err(not_for("it is not an Arm program"));
    }
    let ph_offset = u32_at(0x1c)? as usize;
    let ph_size = u16_at(0x2a)? as usize;
    let ph_count = u16_at(0x2c)? as usize;
    let mut segments = Vec::new();
    for i in 0..ph_count {
        let ph = ph_offset + i * ph_size;
        const PT_LOAD: u32 = 1;
        if u32_at(ph)? != PT_LOAD {
            continue;
        }
        let (offset, paddr) = (u32_at(ph + 4)? as usize, u32_at(ph + 12)?);
        let (file_size, mem_size) = (u32_at(ph + 16)?, u32_at(ph + 20)?);
        let size = file_size.min(mem_size) as usize;
        if size == 0 {
            continue;
        }
        let bytes = elf
            .get(offset..offset + size)
            .ok_or_else(|| "the ELF file is cut short".to_string())?;
        segments.push((paddr, bytes));
    }
    Ok(segments)
}

/// A minimal 32-bit Arm ELF whose load segments put `segments` at their
/// addresses: a stand-in for a program cargo built.
#[doc(hidden)]
pub fn elf32(segments: &[(u32, &[u8])]) -> Vec<u8> {
    const HEADER: usize = 52;
    const PH: usize = 32;
    let mut out = vec![0u8; HEADER];
    out[..4].copy_from_slice(b"\x7fELF");
    out[4] = 1; // 32-bit
    out[5] = 1; // little-endian
    out[6] = 1;
    out[0x10..0x12].copy_from_slice(&2u16.to_le_bytes()); // executable
    out[0x12..0x14].copy_from_slice(&40u16.to_le_bytes()); // Arm
    out[0x14..0x18].copy_from_slice(&1u32.to_le_bytes());
    let entry = segments.first().map_or(0, |s| s.0 | 1);
    out[0x18..0x1c].copy_from_slice(&entry.to_le_bytes());
    out[0x1c..0x20].copy_from_slice(&(HEADER as u32).to_le_bytes());
    out[0x28..0x2a].copy_from_slice(&(HEADER as u16).to_le_bytes());
    out[0x2a..0x2c].copy_from_slice(&(PH as u16).to_le_bytes());
    out[0x2c..0x2e].copy_from_slice(&(segments.len() as u16).to_le_bytes());
    let mut data_at = HEADER + PH * segments.len();
    for (addr, bytes) in segments {
        for word in [
            1,
            data_at as u32,
            *addr,
            *addr,
            bytes.len() as u32,
            bytes.len() as u32,
            5,
            4,
        ] {
            out.extend_from_slice(&u32::to_le_bytes(word));
        }
        data_at += bytes.len();
    }
    for (_, bytes) in segments {
        out.extend_from_slice(bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(block: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap())
    }

    #[test]
    fn families_are_found_by_name_or_id() {
        assert_eq!(Family::named("rp2040"), Some(&RP2040));
        assert_eq!(Family::named("NRF52840"), Some(&NRF52840));
        assert_eq!(Family::named("0xADA52840"), Some(&NRF52840));
        assert_eq!(Family::named("0xe48bff56"), Some(&RP2040));
        assert_eq!(Family::named("samd21"), None);
        assert!(RP2040.is_in("UF2 Bootloader v3.0\nModel: Raspberry Pi RP2\nBoard-ID: RPI-RP2\n"));
        assert!(!NRF52840.is_in("Board-ID: RPI-RP2"));
    }

    #[test]
    fn each_page_is_one_block_with_the_family_id() {
        let code: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let elf = elf32(&[(0x27000, &code), (0x28000, b"data")]);
        let uf2 = from_elf(&elf, &NRF52840).unwrap();
        assert_eq!(uf2.len(), 3 * BLOCK, "two pages of code, one of data");
        let blocks: Vec<&[u8]> = uf2.chunks(BLOCK).collect();
        for (n, b) in blocks.iter().enumerate() {
            assert_eq!(word(b, 0), MAGIC_START0);
            assert_eq!(word(b, 1), MAGIC_START1);
            assert_eq!(word(b, 2), FLAG_FAMILY_ID);
            assert_eq!(word(b, 4), 256);
            assert_eq!(word(b, 5), n as u32);
            assert_eq!(word(b, 6), 3);
            assert_eq!(word(b, 7), 0xada5_2840);
            assert_eq!(word(b, 127), MAGIC_END);
        }
        assert_eq!(
            blocks.iter().map(|b| word(b, 3)).collect::<Vec<_>>(),
            [0x27000, 0x27100, 0x28000],
            "no empty pages between the two"
        );
        assert_eq!(&blocks[0][32..32 + 256], &code[..256]);
        assert_eq!(&blocks[1][32..32 + 44], &code[256..]);
        assert!(blocks[1][32 + 44..].iter().take(476 - 44).all(|&b| b == 0));
        assert_eq!(&blocks[2][32..36], b"data");
    }

    #[test]
    fn an_rp2040_program_fills_the_sectors_it_touches() {
        let elf = elf32(&[(0x1000_0000, &[1; 256]), (0x1000_0800, &[2; 16])]);
        let uf2 = from_elf(&elf, &RP2040).unwrap();
        let addrs: Vec<u32> = uf2.chunks(BLOCK).map(|b| word(b, 3)).collect();
        assert_eq!(
            addrs,
            (0..=8).map(|i| 0x1000_0000 + i * 256).collect::<Vec<_>>(),
            "empty pages up to the last one, none after it"
        );
        assert!(
            uf2.chunks(BLOCK)
                .all(|b| word(b, 7) == 0xe48b_ff56 && word(b, 6) == 9)
        );
    }

    #[test]
    fn the_rp2040_file_is_the_one_elf2uf2_rs_makes() {
        // elf2uf2-rs 2.2.0's own test program and its output (0BSD).
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let elf = std::fs::read(fixtures.join("hello_serial.elf")).unwrap();
        let expected = std::fs::read(fixtures.join("hello_serial.uf2")).unwrap();
        let uf2 = from_elf(&elf, &RP2040).unwrap();
        assert_eq!(uf2.len(), expected.len());
        assert!(uf2 == expected, "byte for byte");
    }

    #[test]
    fn a_program_outside_the_boards_flash_is_refused() {
        let at_zero = elf32(&[(0, &[0; 64])]);
        let err = from_elf(&at_zero, &NRF52840).unwrap_err();
        assert!(
            err.contains("0x00000000") && err.contains("SoftDevice") && err.contains("0x27000"),
            "{err}"
        );
        let err = from_elf(&at_zero, &RP2040).unwrap_err();
        assert!(err.contains("0x10000000"), "{err}");
    }

    #[test]
    fn a_program_for_another_machine_is_refused() {
        let mut host = elf32(&[(0x27000, &[0; 4])]);
        host[4] = 2;
        let err = from_elf(&host, &NRF52840).unwrap_err();
        assert!(err.contains("32-bit"), "{err}");
        let err = from_elf(b"#!/bin/sh\n", &NRF52840).unwrap_err();
        assert!(err.contains("not an ELF"), "{err}");
        let overlap = elf32(&[(0x27000, &[0; 8]), (0x27004, &[0; 8])]);
        assert!(
            from_elf(&overlap, &NRF52840)
                .unwrap_err()
                .contains("overlap")
        );
        let empty = elf32(&[]);
        assert!(from_elf(&empty, &NRF52840).unwrap_err().contains("nothing"));
    }
}
