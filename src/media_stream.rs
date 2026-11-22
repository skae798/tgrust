use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use gtk::{gio, glib};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, watch};

pub const BLOCK_SIZE: i32 = 128 * 1024;
const CACHE_BLOCKS: usize = 16;
type Chunk = Pin<Box<dyn Future<Output = Result<Bytes>> + Send>>;

pub struct StreamSource {
    data: Arc<Source>,
    lifetime: watch::Sender<()>,
}

struct Source {
    size: u64,
    fetch: Box<dyn Fn(i32) -> Chunk + Send + Sync>,
    cache: Mutex<VecDeque<(i32, Bytes)>>,
}

impl StreamSource {
    pub fn new(size: u64, fetch: impl Fn(i32) -> Chunk + Send + Sync + 'static) -> Result<Self> {
        ensure!(
            size > 0 && size.div_ceil(BLOCK_SIZE as u64) <= i32::MAX as u64,
            "Неверный размер медиа"
        );
        Ok(Self {
            data: Arc::new(Source {
                size,
                fetch: Box::new(fetch),
                cache: Mutex::new(VecDeque::new()),
            }),
            lifetime: watch::channel(()).0,
        })
    }

    pub fn cancel(&self) {
        self.lifetime.send_replace(());
    }

    pub fn input(&self, runtime: Handle) -> gio::InputStream {
        use glib::subclass::prelude::ObjectSubclassIsExt;
        let stream: MediaInput = glib::Object::new();
        stream
            .imp()
            .source
            .set(InputSource {
                data: self.data.clone(),
                lifetime: self.lifetime.subscribe(),
                runtime,
            })
            .ok()
            .expect("new input has no source");
        glib::prelude::Cast::upcast(stream)
    }
}

struct InputSource {
    data: Arc<Source>,
    lifetime: watch::Receiver<()>,
    runtime: Handle,
}

impl Source {
    async fn block(&self, index: i32) -> Result<Bytes> {
        let mut cache = self.cache.lock().await;
        if let Some(position) = cache.iter().position(|(current, _)| *current == index) {
            let entry = cache.remove(position).unwrap();
            let bytes = entry.1.clone();
            cache.push_back(entry);
            return Ok(bytes);
        }
        let offset = index as u64 * BLOCK_SIZE as u64;
        let length = (self.size - offset).min(BLOCK_SIZE as u64);
        let bytes = tokio::time::timeout(Duration::from_secs(20), (self.fetch)(index))
            .await
            .context("Не удалось получить фрагмент медиа вовремя")??;
        ensure!(
            bytes.len() as u64 == length,
            "Telegram вернул неполный фрагмент медиа"
        );
        if cache.len() == CACHE_BLOCKS {
            cache.pop_front();
        }
        cache.push_back((index, bytes.clone()));
        Ok(bytes)
    }
}

mod input {
    use super::*;
    use gio::prelude::*;
    use gio::subclass::prelude::*;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    pub struct MediaInput {
        pub(super) source: OnceLock<InputSource>,
        position: AtomicU64,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaInput {
        const NAME: &'static str = "TgrustMediaInput";
        type Type = super::MediaInput;
        type ParentType = gio::InputStream;
        type Interfaces = (gio::Seekable,);
    }

    impl ObjectImpl for MediaInput {}

    fn cancelled() -> glib::Error {
        glib::Error::new(gio::IOErrorEnum::Cancelled, "Чтение медиа отменено")
    }

    impl InputStreamImpl for MediaInput {
        fn read(
            &self,
            buffer: &mut [u8],
            cancellable: Option<&gio::Cancellable>,
        ) -> Result<usize, glib::Error> {
            let source = self
                .source
                .get()
                .expect("stream initialized before playback");
            let offset = self.position.load(Ordering::Relaxed);
            if buffer.is_empty() || offset >= source.data.size {
                return Ok(0);
            }
            if let Some(cancellable) = cancellable {
                cancellable.set_error_if_cancelled()?;
            }
            let index = (offset / BLOCK_SIZE as u64) as i32;
            let start = (offset % BLOCK_SIZE as u64) as usize;
            let mut lifetime = source.lifetime.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let handler = cancellable.and_then(|cancel| {
                cancel.connect_cancelled(move |_| {
                    let _ = tx.send(());
                })
            });
            // GTK's GStreamer backend reads this input on its decoder thread.
            // Fetch only the block containing the requested byte position.
            let data = source.runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = lifetime.changed() => Err(cancelled()),
                    _ = rx, if cancellable.is_some() => Err(cancelled()),
                    block = source.data.block(index) => block.map_err(|error| {
                        glib::Error::new(gio::IOErrorEnum::Failed, &format!("{error:#}"))
                    }),
                }
            });
            if let (Some(cancellable), Some(handler)) = (cancellable, handler) {
                cancellable.disconnect_cancelled(handler);
            }
            let data = data?;
            let length = buffer.len().min(data.len() - start);
            self.position
                .compare_exchange(
                    offset,
                    offset + length as u64,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .map_err(|_| cancelled())?;
            buffer[..length].copy_from_slice(&data[start..start + length]);
            Ok(length)
        }
    }

    impl SeekableImpl for MediaInput {
        fn tell(&self) -> i64 {
            self.position.load(Ordering::Relaxed) as i64
        }

        fn can_seek(&self) -> bool {
            true
        }

        fn seek(
            &self,
            offset: i64,
            type_: glib::SeekType,
            cancellable: Option<&gio::Cancellable>,
        ) -> Result<(), glib::Error> {
            if let Some(cancellable) = cancellable {
                cancellable.set_error_if_cancelled()?;
            }
            let size = self
                .source
                .get()
                .expect("stream initialized before playback")
                .data
                .size as i64;
            let origin = match type_ {
                glib::SeekType::Set => 0,
                glib::SeekType::Cur => self.tell(),
                glib::SeekType::End => size,
                _ => -1,
            };
            let position = origin
                .checked_add(offset)
                .filter(|position| origin >= 0 && (0..=size).contains(position))
                .ok_or_else(|| {
                    glib::Error::new(gio::IOErrorEnum::InvalidArgument, "Неверная позиция медиа")
                })?;
            self.position.store(position as u64, Ordering::Relaxed);
            Ok(())
        }

        fn can_truncate(&self) -> bool {
            false
        }

        fn truncate(&self, _: i64, _: Option<&gio::Cancellable>) -> Result<(), glib::Error> {
            Err(glib::Error::new(
                gio::IOErrorEnum::NotSupported,
                "Поток только для чтения",
            ))
        }
    }
}

glib::wrapper! {
    pub struct MediaInput(ObjectSubclass<input::MediaInput>) @extends gio::InputStream, @implements gio::Seekable;
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn fetches_only_requested_blocks_and_reuses_them_for_seeks() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let fetches = calls.clone();
        let source = StreamSource::new(BLOCK_SIZE as u64 * 4, move |index| {
            fetches.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { Ok(Bytes::from(vec![index as u8; BLOCK_SIZE as usize])) })
        })
        .unwrap();
        let stream = source.input(runtime.handle().clone());
        let seek = stream.dynamic_cast_ref::<gio::Seekable>().unwrap();
        let mut data = [0; 2];
        for _ in 0..2 {
            seek.seek(
                i64::from(BLOCK_SIZE) * 2 + 7,
                glib::SeekType::Set,
                None::<&gio::Cancellable>,
            )
            .unwrap();
            assert_eq!(
                stream.read(&mut data, None::<&gio::Cancellable>).unwrap(),
                2
            );
            assert_eq!(data, [2, 2]);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(
            seek.seek(1, glib::SeekType::End, None::<&gio::Cancellable>)
                .is_err()
        );
        seek.seek(-2, glib::SeekType::End, None::<&gio::Cancellable>)
            .unwrap();
        assert_eq!(
            stream.read(&mut data, None::<&gio::Cancellable>).unwrap(),
            2
        );
        assert_eq!(data, [3, 3]);
        assert_eq!(
            stream.read(&mut data, None::<&gio::Cancellable>).unwrap(),
            0
        );
        seek.seek(0, glib::SeekType::Set, None::<&gio::Cancellable>)
            .unwrap();
        drop(source);
        assert!(
            stream
                .read(&mut data, None::<&gio::Cancellable>)
                .unwrap_err()
                .matches(gio::IOErrorEnum::Cancelled)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }
}
