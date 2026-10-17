// Generates the executable icon from the same procedural art the app uses at runtime.

#[allow(dead_code)]
mod icon_art {
    include!("src/icon_art.rs");
}

use std::io::Write;

fn main() {
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out_dir.join("app.ico");
    write_ico(&ico_path, &[16, 24, 32, 48, 64, 256]);

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().unwrap());
    res.set("FileDescription", "LoudControl");
    res.set("ProductName", "LoudControl");
    res.compile().expect("compile Windows resources");
}

/// Writes a multi-size .ico with 32-bit BMP entries.
fn write_ico(path: &std::path::Path, sizes: &[u32]) {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_entry(s)).collect();
    let mut f = std::fs::File::create(path).unwrap();
    let count = sizes.len() as u16;
    f.write_all(&[0, 0, 1, 0]).unwrap();
    f.write_all(&count.to_le_bytes()).unwrap();
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0u8 } else { s as u8 };
        f.write_all(&[dim, dim, 0, 0]).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&32u16.to_le_bytes()).unwrap();
        f.write_all(&(img.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&offset.to_le_bytes()).unwrap();
        offset += img.len() as u32;
    }
    for img in &images {
        f.write_all(img).unwrap();
    }
}

fn bmp_entry(size: u32) -> Vec<u8> {
    let rgba = icon_art::render_icon(size, icon_art::IconState::Normal);
    let mut v = Vec::new();
    // BITMAPINFOHEADER; height is doubled to account for the (empty) AND mask.
    v.extend_from_slice(&40u32.to_le_bytes());
    v.extend_from_slice(&(size as i32).to_le_bytes());
    v.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&32u16.to_le_bytes());
    v.extend_from_slice(&[0u8; 24]);
    // Pixels bottom-up, BGRA.
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            v.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    // AND mask: 1 bit per pixel, rows padded to 32 bits; all zero (alpha does the work).
    let row = size.div_ceil(32) * 4;
    v.extend(std::iter::repeat_n(0u8, (row * size) as usize));
    v
}
