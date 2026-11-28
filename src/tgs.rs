use std::io::Read;
use std::sync::{Arc, LazyLock, mpsc};

use anyhow::{Context, Result, bail, ensure};
use tokio::sync::{Semaphore, oneshot};

const MAX_COMPRESSED: usize = 64 * 1024;
const MAX_JSON: usize = 4 * 1024 * 1024;
const SIDE: usize = 160;
static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub frames: usize,
    pub fps: f64,
}

struct Request {
    frame: usize,
    reply: oneshot::Sender<Vec<u8>>,
}

struct Worker {
    sender: Option<mpsc::SyncSender<Request>>,
    task: Option<tokio::task::JoinHandle<()>>,
    runtime: tokio::runtime::Handle,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(task) = self.task.take() {
            self.runtime.spawn(async move {
                let _ = task.await;
            });
        }
    }
}

pub struct Renderer {
    worker: Worker,
    pub timing: Timing,
}

impl Renderer {
    pub async fn open(compressed: Vec<u8>) -> Result<Self> {
        Self::with_limit(compressed, WORKERS.clone()).await
    }

    async fn with_limit(compressed: Vec<u8>, limit: Arc<Semaphore>) -> Result<Self> {
        ensure!(compressed.len() <= MAX_COMPRESSED, "TGS превышает 64 КиБ");
        // Admission precedes spawn_blocking; waiting/cancelled clients cannot fill its queue.
        let permit = limit.acquire_owned().await?;
        let (sender, receiver) = mpsc::sync_channel::<Request>(1);
        let (ready, started) = oneshot::channel();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // The native handle is created, used and destroyed only in this worker.
            let prepared = (|| {
                let json = unpack(&compressed)?;
                let mut animation = rlottie::Animation::from_data(json, "", "")
                    .context("Не удалось прочитать анимацию TGS")?;
                let timing = Timing {
                    frames: animation.totalframe(),
                    fps: animation.framerate(),
                };
                ensure!(
                    animation.size() == rlottie::Size::new(512, 512)
                        && timing.frames > 0
                        && timing.frames <= 180
                        && timing.fps.is_finite()
                        && (1.0..=60.0).contains(&timing.fps)
                        && animation.duration() <= 3.0 + 1.0 / timing.fps,
                    "Недопустимые параметры анимации TGS"
                );
                // Warm the renderer before reporting success; no GTK object crosses this boundary.
                let mut surface = rlottie::Surface::new(rlottie::Size::new(SIDE, SIDE));
                animation.render(0, &mut surface);
                Ok((animation, surface, timing))
            })();
            let (mut animation, mut surface, timing) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if ready.send(Ok(timing)).is_err() {
                return;
            }
            while let Ok(request) = receiver.recv() {
                if request.reply.is_closed() {
                    continue;
                }
                animation.render(request.frame, &mut surface);
                let _ = request.reply.send(surface.data_as_bytes().to_vec());
            }
        });
        // This guard also closes the channel if the caller disappears during initialization.
        let worker = Worker {
            sender: Some(sender),
            task: Some(task),
            runtime: tokio::runtime::Handle::current(),
        };
        let timing = started.await.context("Обработчик TGS остановлен")??;
        Ok(Self { worker, timing })
    }

    /// At most one queued frame; a busy renderer drops admission instead of blocking GTK.
    pub fn request(&self, frame: usize) -> Option<oneshot::Receiver<Vec<u8>>> {
        if frame >= self.timing.frames {
            return None;
        }
        let (reply, receiver) = oneshot::channel();
        self.worker
            .sender
            .as_ref()?
            .try_send(Request { frame, reply })
            .ok()?;
        Some(receiver)
    }

    #[cfg(test)]
    async fn shutdown(mut self) -> Result<()> {
        self.worker.sender.take();
        if let Some(task) = self.worker.task.take() {
            task.await?;
        }
        Ok(())
    }
}

fn unpack(compressed: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !compressed.is_empty() && compressed.len() <= MAX_COMPRESSED,
        "Недопустимый размер TGS"
    );
    let mut json = Vec::new();
    flate2::read::MultiGzDecoder::new(compressed)
        .take((MAX_JSON + 1) as u64)
        .read_to_end(&mut json)
        .context("Повреждённый файл TGS")?;
    ensure!(json.len() <= MAX_JSON, "Распакованный TGS превышает 4 МиБ");
    let value: serde_json::Value =
        serde_json::from_slice(&json).context("Некорректный JSON TGS")?;
    ensure!(
        value["w"].as_u64() == Some(512) && value["h"].as_u64() == Some(512),
        "Размер холста TGS должен быть 512 × 512"
    );
    let fps = value["fr"]
        .as_f64()
        .context("Не задана частота кадров TGS")?;
    let start = value["ip"].as_f64().context("Не задано начало TGS")?;
    let end = value["op"].as_f64().context("Не задан конец TGS")?;
    ensure!(
        (1.0..=60.0).contains(&fps)
            && (0.0..=f64::from(i32::MAX)).contains(&start)
            && end <= f64::from(i32::MAX)
            && end > start
            && (end - start) <= 180.0
            && (end - start) / fps <= 3.0,
        "Недопустимая длительность или частота кадров TGS"
    );
    ensure!(
        value.get("fonts").is_none() && value.get("chars").is_none(),
        "TGS с внешними шрифтами не поддерживается"
    );
    let mut nodes = vec![&value];
    let mut visited = 0;
    while let Some(node) = nodes.pop() {
        visited += 1;
        ensure!(visited <= 100_000, "Слишком сложный TGS");
        match node {
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if (key == "p" || key == "u") && value.is_string() {
                        bail!("TGS со ссылками на внешние изображения не поддерживается");
                    }
                    nodes.push(value);
                }
            }
            serde_json::Value::Array(array) => nodes.extend(array),
            _ => {}
        }
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn rejects_truncation_expansion_bombs_external_files_and_invalid_timing() {
        let demo = include_bytes!("../data/demo-sticker.tgs");
        let json = unpack(demo).unwrap();
        assert!(unpack(&demo[..demo.len() - 6]).is_err());
        assert!(unpack(&gzip(&vec![b' '; MAX_JSON + 1])).is_err());
        assert!(unpack(&vec![0; MAX_COMPRESSED + 1]).is_err());
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        for (key, replacement) in [
            (
                "assets",
                serde_json::json!([{"p":"private.png","u":"/tmp/"}]),
            ),
            ("fr", serde_json::json!(61)),
            ("op", serde_json::json!(181)),
            ("w", serde_json::json!(4096)),
            ("fonts", serde_json::json!({})),
        ] {
            let mut invalid = value.clone();
            invalid[key] = replacement;
            assert!(
                unpack(&gzip(&serde_json::to_vec(&invalid).unwrap())).is_err(),
                "Accepted {key}"
            );
        }
    }

    #[test]
    fn renders_transparent_frames_and_cancels_waiting_workers_without_leaking_permits() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let bytes = include_bytes!("../data/demo-sticker.tgs").to_vec();
            let gate = Arc::new(Semaphore::new(1));
            let renderer = Renderer::with_limit(bytes.clone(), gate.clone())
                .await
                .unwrap();
            let first = renderer.request(0).unwrap().await.unwrap();
            let second = renderer.request(30).unwrap().await.unwrap();
            assert_eq!(first.len(), SIDE * SIDE * 4);
            assert_eq!(first[3], 0);
            let center = (SIDE / 2 * SIDE + SIDE / 2) * 4;
            assert!(first[center + 3] > 200);
            assert!(
                first[center + 2] > first[center],
                "The cat should cover its blue background"
            );
            assert!(
                first != second,
                "Animation never changed its frame: {:?}",
                renderer.timing
            );
            assert!(renderer.request(renderer.timing.frames).is_none());
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(20),
                    Renderer::with_limit(bytes.clone(), gate.clone())
                )
                .await
                .is_err()
            );
            assert_eq!(gate.available_permits(), 0);
            renderer.shutdown().await.unwrap();
            assert_eq!(gate.available_permits(), 1);
            let renderer = Renderer::with_limit(bytes, gate.clone()).await.unwrap();
            renderer.shutdown().await.unwrap();
            assert_eq!(gate.available_permits(), 1);
            assert!(Renderer::with_limit(vec![0], gate.clone()).await.is_err());
            // Native initialization has already completed and its permit is returned on failure.
            let permit = tokio::time::timeout(Duration::from_secs(2), gate.acquire())
                .await
                .unwrap()
                .unwrap();
            drop(permit);
        });
    }
}
