//! SDK executable-format preflight. This does not establish vendor identity,
//! authenticate an account, or enable a platform's playback runtime.
use anyhow::{ensure, Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    LinuxAarch64,
    WindowsX86_64,
}

/// Validate bytes already obtained through the caller's protected, bounded read.
pub fn validate(bytes: &[u8], target: Target) -> Result<()> {
    match target {
        Target::LinuxAarch64 => validate_elf(bytes),
        Target::WindowsX86_64 => validate_pe(bytes),
    }
}

// Adapted from tools/soloist-elfuse under the project's MIT license.
fn validate_elf(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 64 && &bytes[..7] == b"\x7fELF\x02\x01\x01",
        "SDK must be an ELF64 little-endian executable"
    );
    let kind = u16::from_le_bytes(bytes[16..18].try_into()?);
    let machine = u16::from_le_bytes(bytes[18..20].try_into()?);
    let version = u32::from_le_bytes(bytes[20..24].try_into()?);
    let offset = u64::from_le_bytes(bytes[32..40].try_into()?);
    let header_size = u16::from_le_bytes(bytes[52..54].try_into()?);
    let entry_size = u16::from_le_bytes(bytes[54..56].try_into()?);
    let count = u16::from_le_bytes(bytes[56..58].try_into()?);
    ensure!(
        matches!(kind, 2 | 3)
            && machine == 183
            && version == 1
            && header_size == 64
            && entry_size == 56
            && count != 0
            && offset >= 64
            && offset
                .checked_add(u64::from(count) * 56)
                .is_some_and(|end| end <= bytes.len() as u64),
        "SDK must be a bounded AArch64 executable with valid ELF program headers"
    );
    Ok(())
}

fn validate_pe(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 64 && &bytes[..2] == b"MZ",
        "SDK must have a complete PE DOS header"
    );
    let offset = u32::from_le_bytes(bytes[60..64].try_into()?) as usize;
    ensure!(offset >= 64, "PE header overlaps the DOS header");
    let coff_end = offset
        .checked_add(24)
        .context("PE header offset overflow")?;
    let coff = bytes.get(offset..coff_end).context("truncated PE header")?;
    ensure!(&coff[..4] == b"PE\0\0", "invalid PE signature");
    let machine = u16::from_le_bytes(coff[4..6].try_into()?);
    let sections = u16::from_le_bytes(coff[6..8].try_into()?);
    let optional_size = u16::from_le_bytes(coff[20..22].try_into()?);
    let characteristics = u16::from_le_bytes(coff[22..24].try_into()?);
    ensure!(
        machine == 0x8664 && characteristics & 2 != 0 && characteristics & 0x2000 == 0,
        "SDK must be an x86_64 PE executable, not a DLL"
    );
    // The Windows loader limits PE images to 96 sections.
    ensure!(
        (1..=96).contains(&sections) && optional_size >= 112,
        "invalid PE section or optional-header size"
    );
    let optional_end = coff_end
        .checked_add(usize::from(optional_size))
        .context("PE optional-header overflow")?;
    let optional = bytes
        .get(coff_end..optional_end)
        .context("truncated PE optional header")?;
    ensure!(
        u16::from_le_bytes(optional[..2].try_into()?) == 0x20b,
        "SDK must use PE32+ headers"
    );
    let table_end = optional_end
        .checked_add(usize::from(sections) * 40)
        .context("PE section-table overflow")?;
    let table = bytes
        .get(optional_end..table_end)
        .context("truncated PE section table")?;
    let headers_size = u32::from_le_bytes(optional[60..64].try_into()?) as usize;
    ensure!(
        headers_size >= table_end && headers_size <= bytes.len(),
        "invalid PE file-header bounds"
    );
    for section in table.chunks_exact(40) {
        let size = u32::from_le_bytes(section[16..20].try_into()?) as usize;
        let start = u32::from_le_bytes(section[20..24].try_into()?) as usize;
        ensure!(
            size == 0
                || (start >= headers_size
                    && start
                        .checked_add(size)
                        .is_some_and(|end| end <= bytes.len())),
            "PE section data is outside the executable"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn elf() -> Vec<u8> {
        let mut bytes = vec![0; 120];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&183u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
        bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
        bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        bytes
    }

    fn pe() -> Vec<u8> {
        let mut bytes = vec![0; 528];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[70..72].copy_from_slice(&1u16.to_le_bytes());
        bytes[84..86].copy_from_slice(&112u16.to_le_bytes());
        bytes[86..88].copy_from_slice(&2u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[148..152].copy_from_slice(&512u32.to_le_bytes());
        bytes[216..220].copy_from_slice(&16u32.to_le_bytes());
        bytes[220..224].copy_from_slice(&512u32.to_le_bytes());
        bytes
    }

    #[test]
    fn native_windows_executable_is_admitted_only_for_windows_preflight() {
        assert!(validate(&pe(), Target::WindowsX86_64).is_ok());
        assert!(validate(&pe(), Target::LinuxAarch64).is_err());
        assert!(validate(&elf(), Target::LinuxAarch64).is_ok());
        assert!(validate(&elf(), Target::WindowsX86_64).is_err());
    }

    #[test]
    fn truncated_executables_are_rejected_without_panics() {
        for (bytes, target) in [(elf(), Target::LinuxAarch64), (pe(), Target::WindowsX86_64)] {
            for length in 0..bytes.len() {
                assert!(
                    validate(&bytes[..length], target).is_err(),
                    "length {length}"
                );
            }
        }
    }

    #[test]
    fn windows_preflight_rejects_wrong_machine_dll_and_unbounded_headers() {
        for (offset, value) in [
            (0, 0u32),
            (60, u32::MAX),
            (64, 0),
            (68, 0x14c),
            (70, 0),
            (70, 97),
            (84, 0),
            (84, 65535),
            (86, 0),
            (86, 0x2002),
            (88, 0x10b),
            (148, 1),
            (148, u32::MAX),
            (216, u32::MAX),
            (220, 0),
            (220, u32::MAX),
        ] {
            let mut bytes = pe();
            let width = if matches!(offset, 68 | 70 | 84 | 86 | 88) {
                2
            } else {
                4
            };
            bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
            assert!(
                validate(&bytes, Target::WindowsX86_64).is_err(),
                "offset {offset}, value {value}"
            );
        }
    }
}
