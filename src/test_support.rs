use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Independent scratch directories; tests never use the user's configuration.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "crestron-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// MS-DOS packed date and time, so the fixtures read like real dates.
pub fn stamp(year: u16, month: u16, day: u16, hour: u16, minute: u16) -> (u16, u16) {
    (
        ((year - 1980) << 9) | (month << 5) | day,
        (hour << 11) | (minute << 5),
    )
}

/// One archive member: name, contents, whether to deflate it, and its packed
/// MS-DOS date and time.
pub type Member<'a> = (&'a str, &'a [u8], bool, (u16, u16));

/// A minimal zip writer. The signatures are spelled out rather than shared
/// with the reader, so a fixture cannot agree with a mistake in it.
pub fn zip(entries: &[Member<'_>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut directory = Vec::new();
    for (name, data, deflate, (date, time)) in entries {
        let payload = if *deflate {
            let mut encoder =
                flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        } else {
            data.to_vec()
        };
        let offset = bytes.len() as u32;
        let method: u16 = if *deflate { 8 } else { 0 };
        bytes.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        for value in [20, 0, method, *time, *date] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&payload);

        directory.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        for value in [20, 20, 0, method, *time, *date] {
            directory.extend_from_slice(&value.to_le_bytes());
        }
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        directory.extend_from_slice(&(data.len() as u32).to_le_bytes());
        directory.extend_from_slice(&(name.len() as u16).to_le_bytes());
        for value in [0u16, 0, 0, 0] {
            directory.extend_from_slice(&value.to_le_bytes());
        }
        directory.extend_from_slice(&0u32.to_le_bytes());
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let offset = bytes.len() as u32;
    let size = directory.len() as u32;
    bytes.extend_from_slice(&directory);
    bytes.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    let count = entries.len() as u16;
    for value in [0, 0, count, count] {
        bytes.extend_from_slice(&u16::to_le_bytes(value));
    }
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes
}
