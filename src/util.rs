use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use eframe::epaint::ColorImage;
use image::{codecs::jpeg::JpegDecoder, DynamicImage};

pub struct Frame<'a> {
    pub jpg: &'a [u8],
    pub rgb: ColorImage,
}

/// Saves jpg buffer to ~/Pictures/kcam
pub fn capture(img: &[u8]) -> Result<PathBuf> {
    let picture_dir = dirs::picture_dir().context("cannot find picture directory")?;
    let save_dir = picture_dir.join("kcam");
    fs::create_dir_all(&save_dir)?;

    let mut i: u32 = 0;
    let mut path = save_dir.join(format!("img_{}.jpg", i));

    while path.exists() {
        i += 1;
        path = save_dir.join(format!("img_{}.jpg", i));
    }

    fs::write(&path, img).context("unable to write image")?;
    Ok(path)
}

pub fn decode(jpg_img: &[u8]) -> Result<ColorImage> {
    let de = JpegDecoder::new(jpg_img)?;
    let img = DynamicImage::from_decoder(de)?.to_rgba8();
    let size = [img.width() as _, img.height() as _];
    let egui_img = ColorImage::from_rgba_unmultiplied(size, img.as_flat_samples().as_slice());

    Ok(egui_img)
}
