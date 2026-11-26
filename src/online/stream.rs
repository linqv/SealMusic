//! One anonymous cache supports a forward-only decoder while the response arrives and
//! a random-access decoder once EOF and the byte limit have both been verified.
use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use reqwest::Response;
use rodio::{Decoder, Source};
use tokio::task::JoinHandle;

use super::{Failure, Result};
use crate::{
    library::metadata::{self, TrackDetails},
    slint_types::OnlineIssue,
};

type Audio = Box<dyn Source + Send + Sync>;
type FileAudio = Decoder<BufReader<File>>;

#[derive(Default)]
struct Progress {
    written: u64,
    finished: bool,
    probed: bool,
    failure: Option<Failure>,
    cancelled: bool,
    // Prepared off the audio thread. The streaming source takes it on the first seek.
    file_decoder: Option<FileAudio>,
    seekable: bool,
    duration: Option<Duration>,
    details: Option<TrackDetails>,
}

struct Cache {
    file: File,
    progress: Mutex<Progress>,
    changed: Condvar,
}

struct Reader {
    cache: Arc<Cache>,
    offset: u64,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let state = self
            .cache
            .changed
            .wait_while(self.cache.progress.lock().unwrap(), |p| {
                self.offset >= p.written && !p.finished && !p.cancelled
            })
            .unwrap();
        if state.cancelled || state.failure.is_some() {
            return Err(io::Error::other("media transfer ended"));
        }
        let count = state.written.saturating_sub(self.offset).min(buf.len() as u64) as usize;
        drop(state);
        let n = self.cache.file.read_at(&mut buf[..count], self.offset)?;
        self.offset += n as u64;
        Ok(n)
    }
}

impl Seek for Reader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Current(0) => Ok(self.offset),
            _ => Err(io::ErrorKind::Unsupported.into()),
        }
    }
}

struct PosReader<'a> {
    file: &'a File,
    pos: u64,
    len: u64,
}

impl Read for PosReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self.len.saturating_sub(self.pos).min(buf.len() as u64) as usize;
        if remaining == 0 {
            return Ok(0);
        }
        let n = self.file.read_at(&mut buf[..remaining], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for PosReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.len.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek offset"))?;
        self.pos = target;
        Ok(self.pos)
    }
}

impl Cache {
    fn probe_complete_file(&self, context: &str) {
        // The writer and progressive reader use positional I/O. Only this
        // decoder uses the file cursor, which is still at the beginning.
        let written = self.progress.lock().unwrap().written;
        let embedded = metadata::read_details_from_reader(PosReader {
            file: &self.file,
            pos: 0,
            len: written,
        });
        let embedded =
            (embedded.cover.is_some() || !embedded.lyrics.is_empty()).then_some(embedded);
        let mut decoder = match self.file.try_clone() {
            Ok(f) => match Decoder::try_from(f) {
                Ok(d) => Some(d),
                Err(e) => {
                    log::warn!("Audio decode failed ({context}): {e}");
                    None
                }
            },
            Err(e) => {
                log::warn!("Media tempfile clone error ({context}): {e}");
                None
            }
        };
        let duration = decoder.as_ref().and_then(Source::total_duration);
        let seekable = decoder.as_mut().is_some_and(|audio| audio.try_seek(Duration::ZERO).is_ok());
        let mut p = self.progress.lock().unwrap();
        p.duration = duration;
        p.seekable = seekable;
        p.file_decoder = decoder;
        p.details = embedded;
        p.probed = true;
        self.changed.notify_all();
    }

    fn decoder(self: &Arc<Self>, resume: bool, context: &str) -> Result<Audio> {
        if !resume {
            let reader = Reader {
                cache: self.clone(),
                offset: 0,
            };
            if let Ok(audio) = Decoder::builder().with_data(reader).with_seekable(false).build() {
                return Ok(Box::new(audio));
            }
        }
        // Containers with a trailing index and saved positions require the complete file.
        let mut p = self
            .changed
            .wait_while(self.progress.lock().unwrap(), |p| {
                !p.probed && !p.cancelled && p.failure.is_none()
            })
            .unwrap();
        if let Some(err) = &p.failure {
            return Err(err.clone());
        }
        if p.cancelled {
            return Err((OnlineIssue::Transfer, context.into()));
        }
        p.file_decoder
            .take()
            .map(|audio| Box::new(audio) as Audio)
            .ok_or_else(|| (OnlineIssue::Decode, context.into()))
    }
}

pub struct Stream {
    cache: Arc<Cache>,
    download: JoinHandle<()>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.cache.progress.lock().unwrap().cancelled = true;
        self.cache.changed.notify_all();
        self.download.abort();
    }
}

impl Stream {
    pub fn status(&self) -> Result<(bool, Option<Duration>)> {
        let p = self.cache.progress.lock().unwrap();
        if let Some(err) = &p.failure {
            return Err(err.clone());
        }
        Ok((p.seekable, p.duration))
    }

    pub fn take_details(&self) -> Option<TrackDetails> {
        self.cache.progress.lock().unwrap().details.take()
    }
}

struct StreamAudio {
    audio: Audio,
    cache: Arc<Cache>,
}

impl Iterator for StreamAudio {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        self.audio.next()
    }
}
impl Source for StreamAudio {
    fn current_span_len(&self) -> Option<usize> {
        self.audio.current_span_len()
    }
    fn channels(&self) -> rodio::ChannelCount {
        self.audio.channels()
    }
    fn sample_rate(&self) -> rodio::SampleRate {
        self.audio.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.audio.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> std::result::Result<(), rodio::source::SeekError> {
        let ready = self.cache.progress.lock().unwrap().file_decoder.take();
        if let Some(mut audio) = ready {
            if let Err(err) = audio.try_seek(pos) {
                self.cache.progress.lock().unwrap().file_decoder = Some(audio);
                return Err(err);
            }
            self.audio = Box::new(audio);
            return Ok(());
        }
        self.audio.try_seek(pos)
    }
}

pub struct PreparedAudio {
    pub source: Audio,
    pub stream: Option<Stream>,
}

async fn receive(
    cache: &Arc<Cache>,
    mut response: Response,
    limit: u64,
    context: &str,
) -> Result<()> {
    let transfer = || (OnlineIssue::Transfer, context.to_owned());
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        log::warn!("Media stream error ({context}): {}", e.without_url());
        transfer()
    })? {
        let len = chunk.len() as u64;
        if len > limit - written {
            log::warn!("Media download exceeded limit ({context})");
            return Err((OnlineIssue::Transfer, "256 MiB".into()));
        }
        let dest = cache.clone();
        tokio::task::spawn_blocking(move || dest.file.write_all_at(&chunk, written))
            .await
            .map_err(|e| {
                log::warn!("Media write task failed ({context}): {e}");
                transfer()
            })?
            .map_err(|e| {
                log::warn!("Media tempfile write error ({context}): {e}");
                transfer()
            })?;
        written += len;
        cache.progress.lock().unwrap().written = written;
        cache.changed.notify_all();
    }
    Ok(())
}

pub(super) async fn download_and_decode(
    response: Response,
    context: &str,
    limit: u64,
    resume: bool,
    finished: impl FnOnce() + Send + 'static,
) -> Result<PreparedAudio> {
    if response.content_length().is_some_and(|n| n > limit) {
        log::warn!("Media response Content-Length exceeds limit ({context})");
        return Err((OnlineIssue::Transfer, "256 MiB".into()));
    }
    let cache = Arc::new(Cache {
        file: tempfile::tempfile().map_err(|e| {
            log::warn!("Could not create media tempfile ({context}): {e}");
            (OnlineIssue::Transfer, context.to_owned())
        })?,
        progress: Mutex::default(),
        changed: Condvar::new(),
    });
    let dest = cache.clone();
    let label = context.to_owned();
    let download = tokio::spawn(async move {
        let result = receive(&dest, response, limit, &label).await;
        {
            let mut state = dest.progress.lock().unwrap();
            state.failure = result.err();
            state.finished = true;
        }
        dest.changed.notify_all();
        if dest.progress.lock().unwrap().failure.is_none() {
            let cache = dest.clone();
            let probe_label = label.clone();
            if super::details::run_cover_decode(move || cache.probe_complete_file(&probe_label))
                .await
                .is_err()
            {
                dest.progress.lock().unwrap().probed = true;
                dest.changed.notify_all();
            }
        }
        finished();
    });
    // Hold the guard across decoder initialization too. Aborting a pending load
    // closes the response and wakes the blocking probe even if the server stalls.
    let stream = Stream {
        cache: cache.clone(),
        download,
    };
    let label = context.to_owned();
    let src = cache.clone();
    let audio =
        tokio::task::spawn_blocking(move || src.decoder(resume, &label)).await.map_err(|e| {
            log::warn!("Audio decode task failed ({context}): {e}");
            (OnlineIssue::Decode, context.to_owned())
        })??;
    stream.status()?;
    Ok(PreparedAudio {
        source: Box::new(StreamAudio {
            audio,
            cache,
        }),
        stream: Some(stream),
    })
}
