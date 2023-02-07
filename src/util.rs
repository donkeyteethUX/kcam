use std::{fs, path::PathBuf};

use anyhow::{ensure, Context, Result};
use chrono::Local;
use eframe::epaint::ColorImage;
use image::{codecs::jpeg::JpegDecoder, DynamicImage};
use log::{debug, info};
use v4l::{
    buffer, context::Node, control::Description, prelude::UserptrStream, video::Capture, Device,
    FourCC,
};

pub struct Frame<'a> {
    pub jpg: &'a [u8],
    pub rgb: ColorImage,
}

/// Saves jpg buffer to ~/Pictures/kcam if possible, or the current directory otherwise.
pub fn capture(img: &[u8]) -> Result<PathBuf> {
    let save_img = |parent_dir: PathBuf| -> Result<PathBuf> {
        let save_dir = parent_dir.join("kcam");

        fs::create_dir_all(&save_dir)?;
        let ts = Local::now().format("%Y-%m-%d_%H-%M-%S-%3f");
        let path = save_dir.join(format!("{ts}.jpg"));

        fs::write(&path, img).context("unable to write image")?;
        Ok(path)
    };

    let save_dir = dirs::picture_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join("Pictures")))
        .unwrap_or_default();

    save_img(save_dir).or_else(|_| save_img(PathBuf::default()))
}

pub fn decode(jpg_img: &[u8]) -> Result<ColorImage> {
    let de = JpegDecoder::new(jpg_img)?;
    let img = DynamicImage::from_decoder(de)?.to_rgba8();
    let size = [img.width() as _, img.height() as _];
    let egui_img = ColorImage::from_rgba_unmultiplied(size, img.as_flat_samples().as_slice());

    Ok(egui_img)
}

pub fn get_stream(dev: &mut Device) -> Result<UserptrStream> {
    let mut format = dev.format()?;
    format.fourcc = FourCC::new(b"MJPG");

    let format = dev.set_format(&format).context("failed to set format")?;
    let params = dev.params().context("failed to get device params")?;

    ensure!(
        format.fourcc == FourCC::new(b"MJPG"),
        "Video capture device doesn't support jpg"
    );

    debug!("Active format:\n{}", format);
    debug!("Active parameters:\n{}", params);

    UserptrStream::new(dev, buffer::Type::VideoCapture).context("Failed to begin stream")
}

pub fn check_device(node: &Node) -> bool {
    let check = |node: &Node| -> Result<()> {
        let mut dev = Device::new(node.index()).context("Failed to open video device.")?;
        get_stream(&mut dev).context("Failed to open stream.")?;
        Ok(())
    };

    let res = check(node);

    match &res {
        Ok(()) => info!(
            "Device check passed for {:?} at {:?}",
            node.name(),
            node.path(),
        ),
        Err(e) => info!(
            "Device check failed for {:?} at {:?}: {:?}",
            node.name(),
            node.path(),
            e
        ),
    }

    res.is_ok()
}

/// Query available controls and sort them by type. Sorting improves the layout of control widgets.
pub fn get_descriptors(dev: &Device) -> Vec<Description> {
    let mut ctrl_descriptors = dev.query_controls().unwrap_or_default();
    ctrl_descriptors.sort_by(|a, b| (a.typ as u32).cmp(&(b.typ as u32)));

    ctrl_descriptors
}
