use std::cell::Cell;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};
use gtk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf, PixbufLoader, prelude::*};

const MAX_BYTES: u64 = 20 * 1024 * 1024;
const SIDE: i32 = 512;

pub fn prepare(path: &Path) -> Result<Vec<u8>> {
    let metadata = path.metadata().context("Не удалось открыть файл")?;
    ensure!(metadata.is_file(), "Выберите файл с фотографией");
    ensure!(metadata.len() <= MAX_BYTES, "Фото должно занимать до 20 МБ");
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .context("Не удалось прочитать файл")?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "Фото должно занимать до 20 МБ"
    );
    make_square(&bytes)
}

fn make_square(bytes: &[u8]) -> Result<Vec<u8>> {
    let format = if bytes.starts_with(b"\xff\xd8\xff") {
        "jpeg"
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else {
        bail!("Выберите фото в формате JPEG или PNG");
    };
    let loader = PixbufLoader::with_type(format)?;
    let oversized = Rc::new(Cell::new(false));
    loader.connect_size_prepared({
        let oversized = oversized.clone();
        move |loader, width, height| {
            if i64::from(width) * i64::from(height) > 32_000_000 {
                oversized.set(true);
                loader.set_size(1, 1);
            }
        }
    });
    let written = loader.write(bytes);
    let closed = loader.close();
    ensure!(!oversized.get(), "Фото должно быть до 32 мегапикселей");
    written
        .and(closed)
        .context("Не удалось прочитать изображение")?;
    let image = loader
        .pixbuf()
        .and_then(|image| image.apply_embedded_orientation())
        .context("Не удалось прочитать изображение")?;
    let (width, height) = (image.width(), image.height());
    let side = width.min(height);
    let square = image.new_subpixbuf((width - side) / 2, (height - side) / 2, side, side);
    let rgb = Pixbuf::new(Colorspace::Rgb, false, 8, SIDE, SIDE)
        .context("Не удалось подготовить фотографию")?;
    rgb.fill(0xffffffff);
    let scale = f64::from(SIDE) / f64::from(side);
    square.composite(
        &rgb,
        0,
        0,
        SIDE,
        SIDE,
        0.0,
        0.0,
        scale,
        scale,
        InterpType::Bilinear,
        255,
    );
    Ok(rgb.save_to_bufferv("jpeg", &[("quality", "90")])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::{gio, glib};

    #[test]
    fn crops_wide_photo_and_composites_transparency() -> Result<()> {
        let image = Pixbuf::new(Colorspace::Rgb, true, 8, 120, 40).unwrap();
        image.fill(0xff0000ff);
        let middle = image.new_subpixbuf(40, 0, 40, 40);
        middle.fill(0x00ff00ff);
        middle.new_subpixbuf(0, 20, 40, 20).fill(0x00000000);
        let jpeg = make_square(&image.save_to_bufferv("png", &[])?)?;
        assert!(jpeg.starts_with(b"\xff\xd8\xff"));
        let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(jpeg));
        let decoded = Pixbuf::from_stream(&stream, gio::Cancellable::NONE)?;
        assert_eq!((decoded.width(), decoded.height()), (SIDE, SIDE));
        let bytes = decoded.read_pixel_bytes();
        let top = (decoded.rowstride() * 8) as usize;
        let bottom = (decoded.rowstride() * (SIDE - 8)) as usize;
        assert!(bytes[top] < 10 && bytes[top + 1] > 245 && bytes[top + 2] < 10);
        assert!(bytes[bottom..bottom + 3].iter().all(|&v| v > 245));
        Ok(())
    }

    #[test]
    fn refuses_unsupported_or_invalid_input() {
        assert!(make_square(b"GIF89a").is_err());
        assert!(make_square(b"\xff\xd8\xff").is_err());
    }
}
