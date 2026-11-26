use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use rodio::{
    ChannelCount, SampleRate, Source,
    source::{SeekError, UniformSourceIterator},
};
use rtrb::{Consumer, Producer, RingBuffer};

use super::{
    clock::AudioClock,
    command::{PlayerCommand as Cmd, Request},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Format {
    pub(super) channels: ChannelCount,
    pub(super) rate: SampleRate,
}

impl Format {
    fn block_samples(self) -> usize {
        let channels = usize::from(self.channels.get());
        (self.rate.get() as usize / 100).clamp(1, (8192 / channels).max(1)) * channels
    }
}

#[derive(Default)]
pub(super) struct Playback(Option<TrackAudio>);

struct TrackAudio {
    decoder: Arc<Mutex<Decoder>>,
    worker: thread::Thread,
    pcm: Arc<Pcm>,
}

struct Decoder {
    source: Box<dyn Source + Send>,
    samples: Producer<f32>,
}

struct Pcm {
    format: Format,
    block: usize,
    clock: Arc<AudioClock>,
    volume: AtomicU32,
    buffer: Mutex<Buffer>,
    active: AtomicBool,
    cancelled: AtomicBool,
    eof: AtomicBool,
    ended: AtomicBool,
}

struct Buffer {
    samples: Consumer<f32>,
    had_samples: bool,
    attachment: u64,
    start: Duration,
    frames: u64,
}

impl Pcm {
    fn position(&self, buffer: &Buffer) -> Duration {
        buffer.start
            + Duration::from_secs_f64(buffer.frames as f64 / f64::from(self.format.rate.get()))
    }

    fn decode(&self, decoder: Arc<Mutex<Decoder>>) {
        let channels = usize::from(self.format.channels.get());
        let mut samples = Vec::with_capacity(self.block);
        while !self.cancelled.load(Ordering::Acquire) {
            let mut decoder = decoder.lock().unwrap();
            if !self.active.load(Ordering::Acquire)
                || self.eof.load(Ordering::Acquire)
                || decoder.samples.slots() < self.block
            {
                drop(decoder);
                // A wake arriving before park is kept as a token.
                thread::park();
                continue;
            }

            // Only the decoder and seek share this lock. The audio callback reads the ring directly.
            samples.clear();
            samples.extend(decoder.source.by_ref().take(self.block));
            if self.cancelled.load(Ordering::Acquire) {
                break;
            }
            // Preserve complete interleaved frames, including at the end of a malformed input.
            let len = samples.len() / channels * channels;
            decoder.samples.push_entire_slice(&samples[..len]).unwrap();
            if samples.len() < self.block {
                self.eof.store(true, Ordering::Release);
            }
        }
    }
}

impl Playback {
    pub(super) fn prepare(
        &mut self,
        source: impl Source + Send + 'static,
        clock: Arc<AudioClock>,
        volume: f32,
    ) {
        self.clear();
        let format = Format {
            channels: source.channels(),
            rate: source.sample_rate(),
        };
        let block = format.block_samples();
        let (producer, consumer) = RingBuffer::new(block * 20);
        let decoder = Arc::new(Mutex::new(Decoder {
            source: Box::new(UniformSourceIterator::new(source, format.channels, format.rate)),
            samples: producer,
        }));
        let pcm = Arc::new(Pcm {
            format,
            block,
            clock,
            volume: AtomicU32::new(volume.to_bits()),
            buffer: Mutex::new(Buffer {
                samples: consumer,
                had_samples: false,
                attachment: 0,
                start: Duration::ZERO,
                frames: 0,
            }),
            active: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        let reader = decoder.clone();
        let writer = pcm.clone();
        let worker = thread::spawn(move || writer.decode(reader)).thread().clone();
        self.0 = Some(TrackAudio {
            decoder,
            worker,
            pcm,
        });
    }

    pub(super) fn format(&self) -> Option<Format> {
        self.0.as_ref().map(|audio| audio.pcm.format)
    }

    pub(super) fn empty(&self) -> bool {
        self.0.as_ref().is_none_or(|audio| audio.pcm.ended.load(Ordering::Acquire))
    }

    pub(super) fn attach(
        &self,
        generation: u64,
        tx: Weak<mpsc::Sender<Request>>,
        drain: Duration,
    ) -> impl Source + Send + 'static {
        let audio = self.0.as_ref().unwrap();
        let pcm = audio.pcm.clone();
        let attachment = {
            let mut buffer = pcm.buffer.lock().unwrap();
            buffer.attachment = buffer.attachment.wrapping_add(1);
            pcm.active.store(true, Ordering::Release);
            buffer.attachment
        };
        audio.worker.unpark();
        PcmSource {
            frame: vec![0.0; usize::from(pcm.format.channels.get())],
            channel: usize::from(pcm.format.channels.get()),
            pcm,
            worker: audio.worker.clone(),
            attachment,
            generation,
            tx,
            drain,
            drain_until: None,
            finished: false,
            primed: false,
        }
    }

    pub(super) fn pause(&self) {
        if let Some(audio) = &self.0 {
            let buffer = audio.pcm.buffer.lock().unwrap();
            audio.pcm.active.store(false, Ordering::Release);
            audio.pcm.clock.store(audio.pcm.position(&buffer));
        }
    }

    pub(super) fn clear(&mut self) {
        if let Some(audio) = self.0.take() {
            audio.pcm.cancelled.store(true, Ordering::Release);
            audio.worker.unpark();
        }
    }

    pub(super) fn try_seek(&self, position: Duration) -> Result<(), SeekError> {
        if let Some(audio) = &self.0 {
            let mut decoder = audio.decoder.lock().unwrap();
            decoder.source.try_seek(position)?;
            let mut buffer = audio.pcm.buffer.lock().unwrap();
            buffer.attachment = buffer.attachment.wrapping_add(1);
            let available = buffer.samples.slots();
            buffer.samples.read_chunk(available).unwrap().commit_all();
            buffer.start = position;
            buffer.frames = 0;
            audio.pcm.eof.store(false, Ordering::Release);
            audio.pcm.ended.store(false, Ordering::Release);
            buffer.had_samples |= !position.is_zero();
            audio.pcm.clock.store(position);
            audio.worker.unpark();
        }
        Ok(())
    }

    pub(super) fn set_volume(&self, volume: f32) {
        if let Some(audio) = &self.0 {
            audio.pcm.volume.store(volume.to_bits(), Ordering::Relaxed);
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.clear();
    }
}

struct PcmSource {
    pcm: Arc<Pcm>,
    worker: thread::Thread,
    frame: Vec<f32>,
    channel: usize,
    attachment: u64,
    generation: u64,
    tx: Weak<mpsc::Sender<Request>>,
    drain: Duration,
    drain_until: Option<Instant>,
    finished: bool,
    primed: bool,
}

impl Iterator for PcmSource {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        if self.finished {
            return None;
        }
        if self.channel == self.frame.len() {
            self.frame.fill(0.0);
            if let Ok(mut buffer) = self.pcm.buffer.try_lock() {
                if self.pcm.cancelled.load(Ordering::Acquire)
                    || !self.pcm.active.load(Ordering::Acquire)
                    || buffer.attachment != self.attachment
                {
                    self.finished = true;
                    return None;
                }
                // EOF is published after the last block. Read it before checking for samples.
                let eof = self.pcm.eof.load(Ordering::Acquire);
                self.primed = self.primed || eof || buffer.samples.slots() >= self.pcm.block * 3;
                if self.primed && buffer.samples.pop_entire_slice(&mut self.frame).is_ok() {
                    buffer.frames += 1;
                    buffer.had_samples = true;
                    let interval = u64::from(self.pcm.format.rate.get()).div_ceil(50);
                    if buffer.frames == 1 || buffer.frames.is_multiple_of(interval) {
                        self.pcm.clock.store(self.pcm.position(&buffer));
                    }
                    let block_frames = (self.pcm.block / self.frame.len()) as u64;
                    if buffer.frames.is_multiple_of(block_frames) {
                        self.worker.unpark();
                    }
                } else if eof {
                    // Let the device play queued samples before the worker closes the stream.
                    let now = Instant::now();
                    let until = self.drain_until.get_or_insert(now + self.drain);
                    if !buffer.had_samples || now >= *until {
                        if !self.pcm.ended.load(Ordering::Relaxed) {
                            self.pcm.clock.store(self.pcm.position(&buffer));
                            self.pcm.ended.store(true, Ordering::Release);
                            if let Some(tx) = self.tx.upgrade() {
                                let _ = tx.send(Request {
                                    command: Cmd::TrackEnded {
                                        generation: self.generation,
                                        had_samples: buffer.had_samples,
                                    },
                                    #[cfg(target_os = "linux")]
                                    reply: None,
                                });
                            }
                        }
                        self.finished = true;
                        return None;
                    }
                } else {
                    self.primed = false;
                }
            }
            self.channel = 0;
        }
        let sample = self.frame[self.channel];
        self.channel += 1;
        Some(sample * f32::from_bits(self.pcm.volume.load(Ordering::Relaxed)))
    }
}

impl Source for PcmSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> ChannelCount {
        self.pcm.format.channels
    }

    fn sample_rate(&self) -> SampleRate {
        self.pcm.format.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SlowSource {
        started: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Iterator for SlowSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            if let Some(started) = self.started.take() {
                started.send(()).unwrap();
                self.release.recv().ok()?;
            }
            Some(0.25)
        }
    }

    impl Source for SlowSource {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> ChannelCount {
            ChannelCount::MIN
        }
        fn sample_rate(&self) -> SampleRate {
            SampleRate::new(8000).unwrap()
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn buffered_samples_are_continuous_while_producer_holds_its_lock() {
        use rodio::buffer::SamplesBuffer;

        let mut playback = Playback::default();
        let rate = SampleRate::new(48_000).unwrap();
        let signal =
            (0..4_800).map(|index| 0.25 + (index % 100) as f32 / 800.0).collect::<Vec<_>>();
        playback.prepare(
            SamplesBuffer::new(ChannelCount::MIN, rate, signal.clone()),
            Arc::new(AudioClock::new(Duration::ZERO)),
            1.0,
        );
        let mut output = playback.attach(1, Weak::new(), Duration::ZERO);
        let audio = playback.0.as_ref().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while audio.pcm.buffer.lock().unwrap().samples.slots() < 2_000 {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        let guard = audio.decoder.lock().unwrap();
        let continuous = signal[..2_000].iter().copied().eq(output.by_ref().take(2_000));
        drop(guard);
        assert!(continuous);
    }

    #[test]
    fn stalled_decoder_allows_output_to_pause_and_reattach_without_seeking() {
        let (entered, started) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let clock = Arc::new(AudioClock::new(Duration::ZERO));
        let mut playback = Playback::default();
        playback.prepare(
            SlowSource {
                started: Some(entered),
                release: wait,
            },
            clock.clone(),
            1.0,
        );
        let mut first = playback.attach(1, Weak::new(), Duration::ZERO);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(first.next(), Some(0.0));
        assert_eq!(clock.position(), Duration::ZERO);

        playback.pause();
        let mut resumed = playback.attach(1, Weak::new(), Duration::ZERO);
        assert_eq!(first.next(), None);
        assert_eq!(first.next(), None);
        assert_eq!(resumed.next(), Some(0.0));
        release.send(()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while resumed.next() != Some(0.25) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(clock.position(), Duration::from_micros(125));
    }
}
