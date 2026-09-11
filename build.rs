//! Gives the Windows executable its icon, rastered from the same description
//! the application draws at runtime so the two can never drift apart.
//!
//! Compiling a resource needs a resource compiler from the Windows SDK. Where
//! there is not one, the build says so and carries on: the icon is missing from
//! the file on disk, and nothing else about the program changes.

use std::{env, fs, io::Write as _, path::PathBuf};

include!("src/logo.rs");

/// The sizes Windows picks between, from a list view up to the largest tile.
const SIZES: [usize; 6] = [16, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("logo.ico");
    if let Err(error) = fs::write(&icon, ico()) {
        println!("cargo:warning=Could not write the application icon: {error}");
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(&icon.to_string_lossy());
    if let Err(error) = resource.compile() {
        println!(
            "cargo:warning=The executable has no icon: {error}. \
             A resource compiler from the Windows SDK is needed to attach one."
        );
    }
}

/// An icon file holding every size. The largest is a PNG, which is what
/// Windows has expected at that size since Vista and what keeps the resource
/// from being mostly one flat colour; the rest are plain bitmaps.
fn ico() -> Vec<u8> {
    let images: Vec<Vec<u8>> = SIZES
        .iter()
        .map(|size| {
            if *size >= 256 {
                png(*size)
            } else {
                bitmap(*size)
            }
        })
        .collect();
    let mut file = Vec::new();
    file.extend_from_slice(&0u16.to_le_bytes()); // reserved
    file.extend_from_slice(&1u16.to_le_bytes()); // an icon, not a cursor
    file.extend_from_slice(&(images.len() as u16).to_le_bytes());
    // Entries come first, so the images start after all of them.
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, image) in SIZES.iter().zip(&images) {
        // 256 does not fit in a byte and is written as zero.
        let side = u8::try_from(*size).unwrap_or(0);
        file.extend_from_slice(&[side, side, 0, 0]);
        file.extend_from_slice(&1u16.to_le_bytes()); // planes
        file.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        file.extend_from_slice(&(image.len() as u32).to_le_bytes());
        file.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        file.extend(image);
    }
    file
}

/// One icon image: a bitmap header, the pixels bottom-up as BGRA, and the
/// obsolete one-bit mask, which is left empty because the alpha channel says
/// everything about what is drawn.
fn bitmap(size: usize) -> Vec<u8> {
    let pixels = rasterize(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut image = Vec::with_capacity(40 + pixels.len() + mask_row * size);
    image.extend_from_slice(&40u32.to_le_bytes()); // header size
    image.extend_from_slice(&(size as i32).to_le_bytes());
    // Doubled: the header describes the pixels and the mask together.
    image.extend_from_slice(&(size as i32 * 2).to_le_bytes());
    image.extend_from_slice(&1u16.to_le_bytes()); // planes
    image.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    image.extend_from_slice(&0u32.to_le_bytes()); // uncompressed
    image.extend_from_slice(&((pixels.len() + mask_row * size) as u32).to_le_bytes());
    image.extend_from_slice(&[0; 16]); // resolution and palette, all unused
    for row in (0..size).rev() {
        for pixel in pixels[row * size * 4..(row + 1) * size * 4].chunks_exact(4) {
            image.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    image.resize(image.len() + mask_row * size, 0);
    image
}

/// One icon image as a PNG: the pixels straight through, one unfiltered row at
/// a time, wrapped in the three chunks a reader needs.
fn png(size: usize) -> Vec<u8> {
    let pixels = rasterize(size);
    let mut rows = Vec::with_capacity((size * 4 + 1) * size);
    for row in 0..size {
        rows.push(0); // this row is stored as it is, not as a difference
        rows.extend_from_slice(&pixels[row * size * 4..(row + 1) * size * 4]);
    }
    let mut deflate = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    deflate.write_all(&rows).expect("writing to a vector");
    let compressed = deflate.finish().expect("writing to a vector");

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&(size as u32).to_be_bytes());
    header.extend_from_slice(&(size as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bits per channel, colour with alpha
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &compressed);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut checked = kind.to_vec();
    checked.extend_from_slice(data);
    png.extend_from_slice(&crc32(&checked).to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 * (crc & 1));
        }
    }
    !crc
}
