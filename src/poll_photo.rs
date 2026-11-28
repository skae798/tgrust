use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;

const MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_PIXELS: u64 = 32_000_000;
static PREPARATIONS: Semaphore = Semaphore::const_new(2);

#[derive(Clone)]
pub struct Photo {
    name: String,
    bytes: Arc<[u8]>,
    preview: Arc<[u8]>,
    spoiler: bool,
}

// Retry identity follows the uploaded payload; the local filename is presentation.
impl PartialEq for Photo {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes && self.spoiler == other.spoiler
    }
}
impl Eq for Photo {}

impl Photo {
    pub fn spoiler(&self) -> bool {
        self.spoiler
    }
    pub fn with_spoiler(mut self, spoiler: bool) -> Self {
        self.spoiler = spoiler;
        self
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn preview(&self) -> &[u8] {
        &self.preview
    }
}

pub async fn load(path: PathBuf) -> Result<Photo> {
    let permit = PREPARATIONS.acquire().await?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .context("Не удалось открыть фото")?;
    ensure!(metadata.is_file(), "Выберите обычный файл с фото");
    ensure!(
        metadata.len() <= MAX_BYTES as u64,
        "Фото должно занимать до 20 МБ"
    );
    let file = tokio::fs::File::open(&path).await?;
    ensure!(
        file.metadata().await?.is_file(),
        "Выберите обычный файл с фото"
    );
    let mut bytes = Vec::new();
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    let name = crate::storage::attachment_name(
        &path.file_name().unwrap_or_default().to_string_lossy(),
        "Фото",
    );
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        prepare(name, bytes)
    })
    .await?
}

fn reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)
        ),
        "Выберите фото JPEG, PNG или WebP"
    );
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    Ok(reader)
}

pub(crate) fn prepare(name: String, bytes: Vec<u8>) -> Result<Photo> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_BYTES,
        "Фото должно занимать от 1 байта до 20 МБ"
    );
    let (width, height) = reader(&bytes)?
        .into_dimensions()
        .context("Не удалось прочитать размер фото")?;
    ensure!(
        width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_PIXELS,
        "Фото должно быть до 32 мегапикселей"
    );
    let mut decoder = reader(&bytes)?.into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder).context("Не удалось прочитать фото")?;
    image.apply_orientation(orientation);
    let image = if image.width() > 2560 || image.height() > 2560 {
        image.thumbnail(2560, 2560)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let rgb = image::RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y);
        let alpha = u16::from(pixel[3]);
        image::Rgb([0, 1, 2].map(|channel| {
            ((u16::from(pixel[channel]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
        }))
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90).encode_image(&rgb)?;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "Подготовленное фото должно занимать до 8 МБ"
    );
    let preview = image::imageops::thumbnail(&rgb, rgb.width().min(640), rgb.height().min(640));
    let mut encoded_preview = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded_preview, 80)
        .encode_image(&preview)?;
    Ok(Photo {
        name,
        bytes: bytes.into(),
        preview: encoded_preview.into(),
        spoiler: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn png() -> Vec<u8> {
        let image = image::RgbaImage::from_fn(80, 40, |x, _| {
            if x < 20 {
                image::Rgba([255, 0, 0, 0])
            } else {
                image::Rgba([0, 100, 240, 255])
            }
        });
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn photo_keeps_aspect_ratio_and_flattens_transparency_without_cropping() {
        let photo = prepare("sample.png".into(), png()).unwrap();
        assert_eq!(photo.name(), "sample.png");
        let decoded = image::load_from_memory(photo.bytes()).unwrap();
        assert_eq!(decoded.dimensions(), (80, 40));
        assert!(
            decoded
                .to_rgb8()
                .get_pixel(0, 0)
                .0
                .iter()
                .all(|channel| *channel > 245)
        );
        assert_eq!(
            image::load_from_memory(photo.preview())
                .unwrap()
                .dimensions(),
            (80, 40)
        );
    }

    #[test]
    fn applies_exif_orientation_and_keeps_spoiler_metadata_outside_the_pixels() {
        use image::ImageEncoder;
        let rgb = image::RgbImage::from_pixel(80, 40, image::Rgb([10, 80, 160]));
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90);
        encoder.set_exif_metadata(b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00".to_vec()).unwrap();
        encoder.encode_image(&rgb).unwrap();
        let photo = prepare("oriented.jpg".into(), bytes).unwrap();
        assert_eq!(
            image::load_from_memory(photo.bytes()).unwrap().dimensions(),
            (40, 80)
        );
        let hidden = photo.clone().with_spoiler(true);
        assert!(!photo.spoiler() && hidden.spoiler());
        assert!(photo != hidden);
        assert_eq!(photo.bytes(), hidden.bytes());
    }

    #[test]
    fn local_rename_does_not_turn_a_retry_into_a_second_send() {
        let first = prepare("first.png".into(), png()).unwrap();
        let renamed = prepare("renamed.png".into(), png()).unwrap();
        assert!(first == renamed);
        assert!(first != renamed.with_spoiler(true));
    }

    #[test]
    fn refuses_invalid_and_oversized_inputs_before_upload() {
        assert!(prepare("empty.png".into(), Vec::new()).is_err());
        assert!(prepare("fake.jpg".into(), b"not an image".to_vec()).is_err());
        assert!(prepare("large.png".into(), vec![0; MAX_BYTES + 1]).is_err());
    }

    #[test]
    fn selected_photo_is_an_immutable_snapshot_even_if_the_source_changes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("photo.png");
            std::fs::write(&path, png()).unwrap();
            let photo = load(path.clone()).await.unwrap();
            let retry = photo.clone();
            std::fs::write(&path, b"changed after selection").unwrap();
            assert!(photo == retry);
            assert!(image::load_from_memory(photo.bytes()).is_ok());
            assert!(load(path).await.is_err());
            assert!(load(dir.path().to_owned()).await.is_err());
        });
    }
}
