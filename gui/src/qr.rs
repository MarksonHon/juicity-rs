//! QR codes for the node share sheet.
//!
//! Links are encoded with the same crate, error correction level and quiet
//! zone as the CLI's `--qrcode-png` (`common::link::save_qrcode_png`), so a
//! scanned code imports into any client that understands `juicity://` and
//! `ss://` share links.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Quiet zone in modules; the QR spec asks for at least four and scanners
/// need it to find the code.
const QUIET_ZONE: usize = 4;

/// A rendered QR code: one entry per module, `true` for a dark module, with
/// the quiet zone included in the width.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrMask {
    width: usize,
    dark: Vec<bool>,
}

/// Encodes `link` as a QR code.
pub fn mask(link: &str) -> anyhow::Result<QrMask> {
    let code = qrcode::QrCode::new(link.as_bytes())?;
    let modules = code.width();
    let width = modules + 2 * QUIET_ZONE;
    let mut dark = vec![false; width * width];
    for y in 0..modules {
        for x in 0..modules {
            dark[(y + QUIET_ZONE) * width + x + QUIET_ZONE] = code[(x, y)] == qrcode::Color::Dark;
        }
    }
    Ok(QrMask { width, dark })
}

/// One pixel per module: dark modules are opaque, the light modules and the
/// quiet zone stay transparent. The sheet then inks the code and fills the
/// tile from the theme, which is what flips the code in the dark theme. Draw
/// it with `image-rendering: pixelated`; smoothing turns the modules into grey
/// squares scanners have to guess.
pub fn image(mask: &QrMask) -> Image {
    let side = mask.width as u32;
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(side, side);
    for (pixel, module) in buffer.make_mut_slice().iter_mut().zip(&mask.dark) {
        *pixel = Rgba8Pixel {
            r: 0,
            g: 0,
            b: 0,
            a: if *module { 255 } else { 0 },
        };
    }
    Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark_at(mask: &QrMask, x: usize, y: usize) -> bool {
        mask.dark[y * mask.width + x]
    }

    const LINK: &str =
        "juicity://8f3c2a71-5b9e-4d0a-9c6f-2e7b1d4a6c90:pass@tokyo.example.com:443#Tokyo";

    /// `qrcode`'s own renderer is the reference for the module placement and
    /// the quiet zone; comparing every module against it also catches a mask
    /// that is mirrored, which no scanner would read.
    #[test]
    fn mask_matches_the_reference_renderer() {
        let mask = mask(LINK).unwrap();
        let reference = qrcode::QrCode::new(LINK.as_bytes())
            .unwrap()
            .render::<image::Luma<u8>>()
            .module_dimensions(1, 1)
            .build();
        assert_eq!(reference.width() as usize, mask.width);
        for y in 0..mask.width {
            for x in 0..mask.width {
                let dark = reference.get_pixel(x as u32, y as u32)[0] == 0;
                assert_eq!(dark_at(&mask, x, y), dark, "module ({x}, {y})");
            }
        }
    }

    #[test]
    fn image_holds_one_pixel_per_module() {
        let mask = mask(LINK).unwrap();
        let size = image(&mask).size();
        assert_eq!(
            (size.width, size.height),
            (mask.width as u32, mask.width as u32)
        );
    }

    #[test]
    fn a_link_too_long_for_any_version_fails() {
        assert!(mask(&"a".repeat(4000)).is_err());
    }
}
