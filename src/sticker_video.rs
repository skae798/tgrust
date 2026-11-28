use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video::VideoFrameExt;
use tokio::sync::{Semaphore, oneshot};

use crate::model::{PlayableMedia, PlaybackFile, StickerAnimationTiming as Timing};

const MAX_FRAMES: usize = 91;
static PREPARATIONS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));

struct Request {
    frame: usize,
    reply: oneshot::Sender<Vec<u8>>,
}

pub struct Frames {
    pub timing: Timing,
    sender: Option<mpsc::SyncSender<Request>>,
    task: Option<tokio::task::JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
}

impl Frames {
    pub async fn open(file: PlaybackFile) -> Result<Self> {
        Self::with_limits(file, WORKERS.clone(), PREPARATIONS.clone()).await
    }

    async fn with_limits(
        file: PlaybackFile,
        workers: Arc<Semaphore>,
        preparations: Arc<Semaphore>,
    ) -> Result<Self> {
        let resident = workers.acquire_owned().await?;
        let preparation = preparations.acquire_owned().await?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let (sender, receiver) = mpsc::sync_channel::<Request>(1);
        let (ready, started) = oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let prepared = Reader::open(file, &flag);
            drop(preparation);
            let _resident = resident;
            let mut reader = match prepared {
                Ok(reader) => reader,
                Err(error) => {
                    drop(_resident);
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if ready.send(Ok(reader.timing)).is_err() {
                return;
            }
            while let Ok(request) = receiver.recv() {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                if request.reply.is_closed() {
                    continue;
                }
                match reader.frame(request.frame, &flag) {
                    Ok(bytes) => {
                        let _ = request.reply.send(bytes);
                    }
                    Err(_) => break,
                }
            }
            // Reader stops all streaming tasks before the worker returns and closes the file.
        });
        let mut frames = Self {
            timing: Timing {
                frames: 0,
                fps: 30.0,
                width: 0,
                height: 0,
            },
            sender: Some(sender),
            task: Some(task),
            cancelled,
            runtime: tokio::runtime::Handle::current(),
        };
        frames.timing = started
            .await
            .context("Обработчик видеостикера остановлен")??;
        Ok(frames)
    }

    pub fn request(&self, frame: usize) -> Option<oneshot::Receiver<Vec<u8>>> {
        if frame >= self.timing.frames {
            return None;
        }
        let (reply, receiver) = oneshot::channel();
        match self.sender.as_ref()?.try_send(Request { frame, reply }) {
            Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => Some(receiver),
            Err(mpsc::TrySendError::Full(_)) => None,
        }
    }

    #[cfg(test)]
    async fn shutdown(mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
        if let Some(task) = self.task.take() {
            task.await.unwrap();
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
        if let Some(task) = self.task.take() {
            self.runtime.spawn(async move {
                let _ = task.await;
            });
        }
    }
}

struct Stop(gst::Pipeline);

impl Drop for Stop {
    fn drop(&mut self) {
        // NULL is synchronous: streaming stops before native elements and the file are dropped.
        let _ = self.0.set_state(gst::State::Null);
    }
}

struct Reader {
    pipeline: Stop,
    sink: gstreamer_app::AppSink,
    bus: gst::Bus,
    _file: PlaybackFile,
    timing: Timing,
    first: Vec<u8>,
    cursor: usize,
}

impl Reader {
    fn open(file: PlaybackFile, cancelled: &AtomicBool) -> Result<Self> {
        let PlayableMedia::Video { width, height, .. } = file.media else {
            bail!("Это не видеостикер")
        };
        ensure!(
            (1..=512).contains(&width)
                && (1..=512).contains(&height)
                && (1..=256 * 1024).contains(&file.file.metadata()?.len()),
            "Недопустимый размер видеостикера"
        );
        gst::init()?;
        let pipeline = gst::parse::launch(r#"filesrc name=input ! matroskademux ! capsfilter caps="video/x-vp9,width=(int)[1,512],height=(int)[1,512]" ! decodebin force-sw-decoders=true ! videoconvert ! videorate ! video/x-raw,format=BGRA,framerate=30/1,pixel-aspect-ratio=1/1 ! appsink name=frames sync=false max-buffers=1 drop=false enable-last-sample=false"#)?
            .downcast::<gst::Pipeline>().map_err(|_| anyhow::anyhow!("Не удалось создать декодер"))?;
        let stop = Stop(pipeline.clone());
        pipeline.connect_deep_element_added(|_, _, element| {
            if element
                .factory()
                .is_some_and(|factory| factory.name() == "vp9dec")
            {
                element.set_property("threads", 2u32);
            }
        });
        pipeline
            .by_name("input")
            .context("Источник отсутствует")?
            .set_property("location", file.path().to_str().unwrap());
        let sink = pipeline
            .by_name("frames")
            .and_then(|element| element.downcast::<gstreamer_app::AppSink>().ok())
            .context("Приёмник кадров отсутствует")?;
        let bus = pipeline.bus().context("Шина декодера отсутствует")?;
        let mut reader = Self {
            pipeline: stop,
            sink,
            bus,
            _file: file,
            timing: Timing {
                frames: 0,
                fps: 30.0,
                width: 0,
                height: 0,
            },
            first: Vec::new(),
            cursor: 0,
        };
        pipeline.set_state(gst::State::Playing)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        // Count without retaining the clip. This also handles WebM without duration metadata.
        while let Some(sample) = reader.sample(cancelled, deadline)? {
            ensure!(
                reader.timing.frames < MAX_FRAMES,
                "Видеостикер длиннее трёх секунд"
            );
            let info = Self::info(&sample)?;
            if reader.timing.frames == 0 {
                reader.timing.width = info.width() as usize;
                reader.timing.height = info.height() as usize;
                reader.first = pixels(&sample, &info)?;
            } else {
                ensure!(
                    info.width() as usize == reader.timing.width
                        && info.height() as usize == reader.timing.height,
                    "Размер кадра изменился"
                );
            }
            reader.timing.frames += 1;
        }
        ensure!(reader.timing.frames > 0, "В видеостикере нет кадров");
        reader.cursor = reader.timing.frames;
        Ok(reader)
    }

    fn info(sample: &gst::Sample) -> Result<gstreamer_video::VideoInfo> {
        let info = gstreamer_video::VideoInfo::from_caps(
            sample.caps().context("Неизвестный формат кадра")?,
        )?;
        ensure!(
            (1..=512).contains(&info.width())
                && (1..=512).contains(&info.height())
                && info.format() == gstreamer_video::VideoFormat::Bgra,
            "Недопустимый формат кадра"
        );
        Ok(info)
    }

    fn sample(&self, cancelled: &AtomicBool, deadline: Instant) -> Result<Option<gst::Sample>> {
        loop {
            ensure!(!cancelled.load(Ordering::Acquire), "Декодирование отменено");
            ensure!(
                Instant::now() < deadline,
                "Декодер видеостикера не ответил вовремя"
            );
            while let Some(message) = self.bus.pop() {
                if let gst::MessageView::Error(error) = message.view() {
                    bail!("Не удалось прочитать видеостикер. {}", error.error());
                }
            }
            if let Some(sample) = self
                .sink
                .try_pull_sample(gst::ClockTime::from_mseconds(100))
            {
                return Ok(Some(sample));
            }
            if self.sink.is_eos() {
                return Ok(None);
            }
        }
    }

    fn frame(&mut self, target: usize, cancelled: &AtomicBool) -> Result<Vec<u8>> {
        if target == 0 {
            return Ok(self.first.clone());
        }
        if target < self.cursor {
            self.pipeline.0.seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::ClockTime::ZERO,
            )?;
            self.cursor = 0;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let sample = self
                .sample(cancelled, deadline)?
                .context("Видеостикер закончился раньше ожидаемого")?;
            let index = self.cursor;
            self.cursor += 1;
            if index == target {
                let info = Self::info(&sample)?;
                return pixels(&sample, &info);
            }
        }
    }
}

fn pixels(sample: &gst::Sample, info: &gstreamer_video::VideoInfo) -> Result<Vec<u8>> {
    let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(
        sample.buffer().context("Пустой кадр")?,
        info,
    )?;
    let stride = usize::try_from(frame.plane_stride()[0])?;
    let plane = frame.plane_data(0)?;
    let (width, height) = (info.width() as usize, info.height() as usize);
    let mut bytes = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        bytes.extend_from_slice(
            plane
                .get(row * stride..row * stride + width * 4)
                .context("Неполный кадр")?,
        );
    }
    if !info
        .flags()
        .contains(gstreamer_video::VideoFlags::PREMULTIPLIED_ALPHA)
    {
        for pixel in bytes.as_chunks_mut::<4>().0 {
            let alpha = u16::from(pixel[3]);
            for channel in &mut pixel[..3] {
                *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample(bytes: &[u8]) -> PlaybackFile {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        PlaybackFile {
            file,
            media: crate::demo::sticker_video_media(),
        }
    }

    #[test]
    fn decodes_bounded_transparent_frames_and_releases_worker_after_shutdown() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let workers = Arc::new(Semaphore::new(1));
            let preparations = Arc::new(Semaphore::new(1));
            let frames = Frames::with_limits(
                sample(include_bytes!("../data/demo-sticker-video.webm")),
                workers.clone(),
                preparations.clone(),
            )
            .await
            .unwrap();
            assert_eq!(workers.available_permits(), 0);
            assert_eq!(preparations.available_permits(), 1);
            assert!((30..=MAX_FRAMES).contains(&frames.timing.frames));
            assert_eq!((frames.timing.width, frames.timing.height), (512, 512));
            let first = frames.request(0).unwrap().await.unwrap();
            let second = frames.request(15).unwrap().await.unwrap();
            assert_eq!(first.len(), 512 * 512 * 4);
            assert_eq!(first[3], 0);
            assert!(first[(256 * 512 + 256) * 4 + 3] > 200);
            assert!(first != second, "Video frames never changed");
            assert_eq!(frames.request(0).unwrap().await.unwrap(), first);
            assert_eq!(frames.request(15).unwrap().await.unwrap(), second);
            assert!(frames.request(frames.timing.frames).is_none());
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(20),
                    Frames::with_limits(
                        sample(include_bytes!("../data/demo-sticker-video.webm")),
                        workers.clone(),
                        preparations.clone()
                    )
                )
                .await
                .is_err()
            );
            frames.shutdown().await;
            assert_eq!(workers.available_permits(), 1);
            assert!(
                Frames::with_limits(
                    sample(b"invalid synthetic webm"),
                    workers.clone(),
                    preparations.clone()
                )
                .await
                .is_err()
            );
            let permit = tokio::time::timeout(Duration::from_secs(2), workers.acquire())
                .await
                .unwrap()
                .unwrap();
            drop(permit);
            assert_eq!(preparations.available_permits(), 1);
        });
    }

    #[test]
    fn rapid_cancellation_stops_streaming_before_releasing_native_elements() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for _ in 0..24 {
                let file = sample(include_bytes!("../data/demo-sticker-video.webm"));
                let result =
                    tokio::task::spawn_blocking(move || Reader::open(file, &AtomicBool::new(true)))
                        .await
                        .unwrap();
                assert!(result.err().unwrap().to_string().contains("отменено"));
            }
        });
    }
}
