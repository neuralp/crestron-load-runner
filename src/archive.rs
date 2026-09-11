//! What a firmware file says about itself.
//!
//! Both firmware types are zip archives: a `.puf` carries a `~.package.ini`
//! describing the build, and a `.zip` update carries nothing but its files.
//! Only the central directory and, at most, one small entry are ever read, so
//! this stays cheap on a firmware image of any size.

use std::{
    fs::File,
    io::{Read as _, Seek, SeekFrom},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const END_OF_CENTRAL_DIRECTORY: u32 = 0x0605_4b50;
const CENTRAL_FILE_HEADER: u32 = 0x0201_4b50;
const LOCAL_FILE_HEADER: u32 = 0x0403_4b50;
const PACKAGE_ENTRY: &str = "~.package.ini";

/// Nothing legitimate in a package description approaches this, and the cap is
/// what keeps a malformed or hostile archive from being expanded into memory.
const PACKAGE_LIMIT: u64 = 1 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Archive {
    pub entries: usize,
    /// Timestamp of the most recently modified entry, as the archive records
    /// it. Zip keeps wall-clock time with no zone, so it is shown as written.
    pub newest: Option<String>,
    /// The `[Package]` section of `~.package.ini`, in file order. Empty for a
    /// plain zip update, which carries no description.
    pub package: Vec<(String, String)>,
}

pub fn read(path: &Path) -> Result<Archive, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let length = file
        .seek(SeekFrom::End(0))
        .map_err(|error| error.to_string())?;
    // The record sits at the very end, behind a comment of up to 64 KiB.
    let window = length.min(22 + u64::from(u16::MAX)) as usize;
    let mut tail = vec![0; window];
    read_at(&mut file, length - window as u64, &mut tail)?;
    let start = (0..=window.saturating_sub(22))
        .rev()
        .find(|&index| u32_at(&tail, index) == Some(END_OF_CENTRAL_DIRECTORY))
        .ok_or("Not a zip archive: no end-of-central-directory record")?;
    let record = &tail[start..];
    let entries = field(u16_at(record, 10))? as usize;
    let size = u64::from(field(u32_at(record, 12))?);
    let offset = u64::from(field(u32_at(record, 16))?);
    if entries == usize::from(u16::MAX)
        || size == u64::from(u32::MAX)
        || offset == u64::from(u32::MAX)
    {
        return Err("Zip64 archives are not read".into());
    }

    let mut directory = vec![0; size as usize];
    read_at(&mut file, offset, &mut directory)?;
    let mut archive = Archive {
        entries,
        ..Default::default()
    };
    let mut package = None;
    let mut newest = 0u32;
    let mut cursor = 0usize;
    for _ in 0..entries {
        let header = directory
            .get(cursor..)
            .filter(|header| header.len() >= 46)
            .ok_or("Truncated zip central directory")?;
        if u32_at(header, 0) != Some(CENTRAL_FILE_HEADER) {
            return Err("Damaged zip central directory".into());
        }
        let name_length = field(u16_at(header, 28))? as usize;
        let extra_length = field(u16_at(header, 30))? as usize;
        let comment_length = field(u16_at(header, 32))? as usize;
        let name = header
            .get(46..46 + name_length)
            .ok_or("Truncated zip entry name")?;
        // The date occupies the high half so that one comparison orders both.
        let stamp =
            (u32::from(field(u16_at(header, 14))?) << 16) | u32::from(field(u16_at(header, 12))?);
        newest = newest.max(stamp);
        if String::from_utf8_lossy(name)
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|entry| entry.eq_ignore_ascii_case(PACKAGE_ENTRY))
        {
            package = Some(Entry {
                method: field(u16_at(header, 10))?,
                compressed: u64::from(field(u32_at(header, 20))?),
                uncompressed: u64::from(field(u32_at(header, 24))?),
                offset: u64::from(field(u32_at(header, 42))?),
            });
        }
        cursor += 46 + name_length + extra_length + comment_length;
    }
    archive.newest = (newest >> 16 != 0).then(|| timestamp((newest >> 16) as u16, newest as u16));
    if let Some(entry) = package {
        archive.package = package_section(&entry.read(&mut file)?);
    }
    Ok(archive)
}

struct Entry {
    /// Zip's general-purpose bit 3 would put the sizes after the data, but the
    /// central directory always holds the real ones, so they are taken there.
    method: u16,
    compressed: u64,
    uncompressed: u64,
    offset: u64,
}

impl Entry {
    fn read(&self, file: &mut File) -> Result<String, String> {
        if self.uncompressed > PACKAGE_LIMIT || self.compressed > PACKAGE_LIMIT {
            return Err(format!("{PACKAGE_ENTRY} is implausibly large"));
        }
        let mut header = [0; 30];
        read_at(file, self.offset, &mut header)?;
        if u32_at(&header, 0) != Some(LOCAL_FILE_HEADER) {
            return Err("Damaged zip entry header".into());
        }
        let skip = u64::from(field(u16_at(&header, 26))?) + u64::from(field(u16_at(&header, 28))?);
        let mut compressed = vec![0; self.compressed as usize];
        read_at(file, self.offset + 30 + skip, &mut compressed)?;
        let bytes = match self.method {
            0 => compressed,
            8 => {
                let mut bytes = Vec::new();
                flate2::read::DeflateDecoder::new(compressed.as_slice())
                    .take(PACKAGE_LIMIT)
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("Could not expand {PACKAGE_ENTRY}: {error}"))?;
                bytes
            }
            method => return Err(format!("{PACKAGE_ENTRY} uses compression method {method}")),
        };
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Key/value pairs of the `[Package]` section, in file order. Everything
/// outside that section, and every line that is not a `key=value`, is skipped.
fn package_section(text: &str) -> Vec<(String, String)> {
    let mut values = Vec::new();
    let mut inside = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            inside = section.trim().eq_ignore_ascii_case("Package");
        } else if inside
            && let Some((key, value)) = line.split_once('=')
            && !key.trim().is_empty()
        {
            values.push((key.trim().to_owned(), value.trim().to_owned()));
        }
    }
    values
}

/// Zip stores MS-DOS date and time: the date counts years from 1980, and the
/// time counts seconds in twos, which is why seconds are not shown.
fn timestamp(date: u16, time: u16) -> String {
    let (year, month, day) = (1980 + (date >> 9), (date >> 5) & 0xf, date & 0x1f);
    let (hour, minute) = (time >> 11, (time >> 5) & 0x3f);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Sizes the way a file manager shows them, with the exact count kept for the
/// cases where it matters.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = "bytes";
    for next in UNITS {
        if size < 1024.0 {
            break;
        }
        size /= 1024.0;
        unit = next;
    }
    if unit == "bytes" {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {unit} ({bytes} bytes)")
    }
}

/// File times have no zone in `std`, so they are shown as UTC and labelled.
pub fn utc(time: SystemTime) -> Option<String> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Days to a civil date, after Howard Hinnant's civil_from_days.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3_600,
        (rest % 3_600) / 60
    ))
}

fn read_at(file: &mut File, offset: u64, into: &mut [u8]) -> Result<(), String> {
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(into))
        .map_err(|error| format!("Could not read the archive: {error}"))
}

fn field<T>(value: Option<T>) -> Result<T, String> {
    value.ok_or_else(|| "Truncated zip record".to_owned())
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{TestDir, stamp, zip};

    fn written(dir: &TestDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn a_puf_reports_the_package_section_from_its_compressed_description() {
        let dir = TestDir::new();
        let ini = concat!(
            "\u{feff}; a comment\r\n",
            "[Other]\r\nIgnored=yes\r\n\r\n",
            "[Package]\r\n",
            "Name=RMC4\r\n",
            " Version = 2.8001.00049 \r\n",
            "Build Date=2024-03-15\r\n",
            "not a pair\r\n",
            "[Trailing]\r\nAlso=ignored\r\n",
        );
        let path = written(
            &dir,
            "device.puf",
            &zip(&[
                ("firmware.bin", b"payload", false, stamp(2020, 1, 2, 3, 4)),
                (
                    "~.package.ini",
                    ini.as_bytes(),
                    true,
                    stamp(2024, 3, 15, 14, 22),
                ),
            ]),
        );
        let archive = read(&path).unwrap();
        assert_eq!(archive.entries, 2);
        assert_eq!(archive.newest.as_deref(), Some("2024-03-15 14:22"));
        assert_eq!(
            archive.package,
            [
                ("Name".to_owned(), "RMC4".to_owned()),
                ("Version".to_owned(), "2.8001.00049".to_owned()),
                ("Build Date".to_owned(), "2024-03-15".to_owned()),
            ]
        );
    }

    #[test]
    fn a_plain_zip_reports_its_newest_entry_and_no_package() {
        let dir = TestDir::new();
        let path = written(
            &dir,
            "update.zip",
            &zip(&[
                ("one.bin", b"a", false, stamp(2021, 6, 1, 9, 30)),
                ("nested/two.bin", b"bb", false, stamp(2023, 12, 25, 18, 5)),
                ("three.bin", b"ccc", false, stamp(2022, 2, 3, 0, 0)),
            ]),
        );
        let archive = read(&path).unwrap();
        assert_eq!(archive.entries, 3);
        assert_eq!(archive.newest.as_deref(), Some("2023-12-25 18:05"));
        assert!(archive.package.is_empty());
    }

    #[test]
    fn the_package_is_found_in_a_subdirectory_and_when_it_is_not_compressed() {
        let dir = TestDir::new();
        let path = written(
            &dir,
            "nested.puf",
            &zip(&[(
                "meta/~.PACKAGE.INI",
                b"[Package]\nName=TSW-1070\n",
                false,
                stamp(2024, 1, 1, 0, 0),
            )]),
        );
        assert_eq!(
            read(&path).unwrap().package,
            [("Name".to_owned(), "TSW-1070".to_owned())]
        );
    }

    #[test]
    fn damaged_archives_are_reported_rather_than_trusted() {
        let dir = TestDir::new();
        let good = zip(&[("one.bin", b"a", false, stamp(2021, 6, 1, 9, 30))]);
        for (name, bytes) in [
            ("empty.zip", Vec::new()),
            ("text.zip", b"not an archive at all".to_vec()),
            ("truncated.zip", good[..good.len() - 8].to_vec()),
            ("headless.zip", {
                // Keep the trailer but corrupt the central directory it names.
                let mut damaged = good.clone();
                let trailer = damaged.len() - 22;
                let directory = u32_at(&damaged, trailer + 16).unwrap() as usize;
                damaged[directory] ^= 0xff;
                damaged
            }),
        ] {
            let path = written(&dir, name, &bytes);
            assert!(read(&path).is_err(), "{name}");
        }
        assert!(read(&dir.path().join("absent.zip")).is_err());
    }

    #[test]
    fn sizes_and_file_times_are_shown_the_way_a_person_reads_them() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1_023), "1023 bytes");
        assert_eq!(human_size(1_024), "1.0 KB (1024 bytes)");
        assert_eq!(human_size(1_536), "1.5 KB (1536 bytes)");
        assert_eq!(human_size(5_242_880), "5.0 MB (5242880 bytes)");
        assert_eq!(utc(UNIX_EPOCH).as_deref(), Some("1970-01-01 00:00 UTC"));
        assert_eq!(
            utc(UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)).as_deref(),
            Some("2023-11-14 22:13 UTC")
        );
    }
}
