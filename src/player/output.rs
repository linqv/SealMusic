use std::{
    sync::{Weak, mpsc},
    time::{Duration, Instant},
};

use rodio::{
    DeviceSinkBuilder, MixerDeviceSink,
    cpal::{
        self, Error, ErrorKind, SampleFormat,
        traits::{DeviceTrait, HostTrait},
    },
    mixer::Mixer,
};

use super::{
    audio::Format,
    command::{PlayerCommand as Cmd, Request},
};

#[derive(Default)]
pub(super) struct Output {
    generation: u64,
    active: Option<Active>,
    retry_attempt: usize,
    default_config_for: Option<Format>,
    #[cfg(test)]
    test_mixer: Option<Mixer>,
    #[cfg(test)]
    pub(super) fail_open: bool,
}

struct Active {
    requested: Format,
    used_default_fallback: bool,
    mixer: Mixer,
    drain: Duration,
    opened_at: Instant,
    _sink: Option<MixerDeviceSink>,
}

impl Output {
    #[cfg(test)]
    pub(super) fn simulated(mixer: Mixer) -> Self {
        Self {
            test_mixer: Some(mixer),
            ..Self::default()
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn is_open(&self) -> bool {
        self.active.is_some()
    }

    #[cfg(test)]
    pub(super) fn used_default_fallback(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.used_default_fallback)
    }

    pub(super) fn reset_retries(&mut self) {
        self.retry_attempt = 0;
    }

    pub(super) fn next_retry(&mut self) -> Option<Duration> {
        const DELAYS_MS: [u64; 3] = [150, 350, 700];
        let delay = *DELAYS_MS.get(self.retry_attempt)?;
        self.retry_attempt += 1;
        Some(Duration::from_millis(delay))
    }

    pub(super) fn stream_failed(&mut self, kind: ErrorKind) {
        let lived_long = self
            .active
            .as_ref()
            .is_some_and(|active| active.opened_at.elapsed() >= Duration::from_secs(5));
        if lived_long {
            self.reset_retries();
        }
        if let Some(active) = &self.active {
            if !active.used_default_fallback
                && (kind == ErrorKind::UnsupportedConfig
                    || (!lived_long && self.retry_attempt >= 1))
            {
                self.default_config_for = Some(active.requested);
            } else if active.used_default_fallback && !lived_long {
                self.default_config_for = None;
            }
        }
        self.close();
    }

    pub(super) fn close(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.active = None;
    }

    pub(super) fn mixer(
        &mut self,
        format: Format,
        tx: &Weak<mpsc::Sender<Request>>,
    ) -> Result<(&Mixer, Duration), String> {
        if self.active.as_ref().is_none_or(|active| active.requested != format) {
            self.close();
            let use_default = self.default_config_for == Some(format);
            if !use_default {
                self.default_config_for = None;
            }
            #[cfg(test)]
            if self.fail_open {
                return Err("Audio output: no simulated device".into());
            }
            #[cfg(test)]
            if let Some(mixer) = self.test_mixer.as_ref() {
                self.active = Some(Active {
                    requested: format,
                    used_default_fallback: use_default,
                    mixer: mixer.clone(),
                    drain: Duration::ZERO,
                    opened_at: Instant::now(),
                    _sink: None,
                });
            }
            if self.active.is_none() {
                let sink = open(format, !use_default, &mut self.generation, tx)?;
                let config = sink.config();
                let drain = match config.buffer_size() {
                    cpal::BufferSize::Fixed(frames) => {
                        Duration::from_secs_f64(
                            f64::from(*frames) / f64::from(config.sample_rate().get()),
                        ) * 2
                    }
                    cpal::BufferSize::Default => Duration::from_millis(100),
                };
                self.active = Some(Active {
                    requested: format,
                    used_default_fallback: use_default,
                    mixer: sink.mixer().clone(),
                    drain,
                    opened_at: Instant::now(),
                    _sink: Some(sink),
                });
            }
        }
        let active = self.active.as_ref().unwrap();
        Ok((&active.mixer, active.drain))
    }
}

fn open(
    format: Format,
    prefer_native: bool,
    generation: &mut u64,
    tx: &Weak<mpsc::Sender<Request>>,
) -> Result<MixerDeviceSink, String> {
    let host = cpal::default_host();
    let primary = host.id();
    let first_error = match open_on_host(&host, format, prefer_native, generation, tx) {
        Ok(sink) => return Ok(sink),
        Err(err) => err,
    };
    #[cfg(target_os = "linux")]
    for id in [
        #[cfg(feature = "pipewire")]
        cpal::HostId::PipeWire,
        cpal::HostId::PulseAudio,
        cpal::HostId::Alsa,
    ] {
        if id != primary
            && let Ok(host) = cpal::host_from_id(id)
            && let Ok(sink) = open_on_host(&host, format, prefer_native, generation, tx)
        {
            return Ok(sink);
        }
    }
    Err(format!("Audio output: {primary:?}: {first_error}"))
}

fn select_supported_config(
    configs: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
    format: Format,
    default: &cpal::SupportedStreamConfig,
) -> Option<cpal::SupportedStreamConfig> {
    let channels = format.channels.get();
    let rate = format.rate.get();
    configs
        .into_iter()
        .filter(|config| {
            config.min_sample_rate() <= rate
                && config.max_sample_rate() >= rate
                && !(config.channels() == channels && config.sample_format() == SampleFormat::F32)
        })
        .min_by_key(|config| {
            let channel_rank = if config.channels() == channels {
                0
            } else if config.channels() == default.channels() {
                1
            } else if config.channels() >= 2 && channels >= 2 {
                2
            } else {
                3
            };
            (
                channel_rank,
                config.channels().abs_diff(channels),
                match config.sample_format() {
                    SampleFormat::F32 => 0,
                    SampleFormat::I32 | SampleFormat::I24 => 1,
                    kind if kind == default.sample_format() => 2,
                    _ => 3,
                },
            )
        })
        .map(|config| config.with_sample_rate(rate))
}

fn open_on_host(
    host: &cpal::Host,
    format: Format,
    prefer_native: bool,
    generation: &mut u64,
    tx: &Weak<mpsc::Sender<Request>>,
) -> Result<MixerDeviceSink, String> {
    let device = host.default_output_device().ok_or("No default device")?;
    let default = device.default_output_config().map_err(|err| err.to_string())?;
    let channels = format.channels.get();
    let rate = format.rate.get();
    let mut try_open = |config: DeviceSinkBuilder| {
        *generation = generation.wrapping_add(1);
        let generation = *generation;
        let tx = tx.clone();
        let mut sink = config
            .with_error_callback(move |error| {
                if let Some(tx) = tx.upgrade() {
                    let _ = tx.send(Request {
                        command: Cmd::OutputError {
                            generation,
                            error,
                        },
                        #[cfg(target_os = "linux")]
                        reply: None,
                    });
                }
            })
            .open_stream()?;
        sink.log_on_drop(false);
        Ok::<_, rodio::DeviceSinkError>(sink)
    };

    let sink = if prefer_native {
        let mut supported = None;
        // A shared server can accept a client rate outside the graph's advertised rates.
        // Try native source PCM first, and use advertised hardware configurations if it fails.
        let preferred = DeviceSinkBuilder::default()
            .with_device(device.clone())
            .with_channels(format.channels)
            .with_sample_rate(format.rate)
            .with_sample_format(SampleFormat::F32);
        let mut sink = try_open(preferred);
        if sink.is_err() {
            supported = device
                .supported_output_configs()
                .ok()
                .and_then(|configs| select_supported_config(configs, format, &default));
            if let Some(config) = &supported {
                sink = try_open(
                    DeviceSinkBuilder::default()
                        .with_device(device.clone())
                        .with_supported_config(config),
                );
            }
        }
        let default_matches_preferred = default.channels() == channels
            && default.sample_rate() == rate
            && default.sample_format() == SampleFormat::F32;
        match sink {
            Ok(sink) => sink,
            Err(err) if default_matches_preferred || supported.as_ref() == Some(&default) => {
                return Err(format!("{err:?}"));
            }
            Err(_) => try_open(
                DeviceSinkBuilder::default().with_device(device).with_supported_config(&default),
            )
            .map_err(|err| format!("{err:?}"))?,
        }
    } else {
        try_open(DeviceSinkBuilder::default().with_device(device).with_supported_config(&default))
            .map_err(|err| format!("{err:?}"))?
    };
    let config = sink.config();
    log::info!(
        "Audio output {:?}: source {} Hz/{} ch, stream {} Hz/{} ch {}",
        host.id(),
        rate,
        channels,
        config.sample_rate(),
        config.channel_count(),
        config.sample_format(),
    );
    Ok(sink)
}

pub(super) fn needs_recovery(error: &Error) -> bool {
    !matches!(error.kind(), ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied | ErrorKind::Xrun)
}

#[cfg(test)]
mod tests {
    use rodio::{ChannelCount, SampleRate, cpal::SupportedBufferSize};

    use super::*;

    #[test]
    fn select_supported_config_skips_failed_preferred_and_prefers_default_channels_over_mono() {
        let default = cpal::SupportedStreamConfig::new(
            2,
            48_000,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        );
        let stereo_44k = Format {
            channels: ChannelCount::new(2).unwrap(),
            rate: SampleRate::new(44_100).unwrap(),
        };
        let candidates = [
            cpal::SupportedStreamConfigRange::new(
                2,
                44_100,
                48_000,
                SupportedBufferSize::Unknown,
                SampleFormat::F32,
            ),
            cpal::SupportedStreamConfigRange::new(
                2,
                44_100,
                48_000,
                SupportedBufferSize::Unknown,
                SampleFormat::I32,
            ),
        ];
        let picked = select_supported_config(candidates, stereo_44k, &default).unwrap();
        assert_eq!(picked.channels(), 2);
        assert_eq!(picked.sample_rate(), 44_100);
        assert_eq!(picked.sample_format(), SampleFormat::I32);

        let surround_44k = Format {
            channels: ChannelCount::new(6).unwrap(),
            rate: SampleRate::new(44_100).unwrap(),
        };
        let multi_candidates = [
            cpal::SupportedStreamConfigRange::new(
                1,
                44_100,
                48_000,
                SupportedBufferSize::Unknown,
                SampleFormat::F32,
            ),
            cpal::SupportedStreamConfigRange::new(
                2,
                44_100,
                48_000,
                SupportedBufferSize::Unknown,
                SampleFormat::F32,
            ),
        ];
        let picked = select_supported_config(multi_candidates, surround_44k, &default).unwrap();
        assert_eq!(picked.channels(), 2);
        assert_eq!(picked.sample_rate(), 44_100);
        assert_eq!(picked.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn output_falls_back_to_default_config_on_async_unsupported_config_and_resets_on_new_format() {
        let (mixer, _source) =
            rodio::mixer::mixer(ChannelCount::new(2).unwrap(), SampleRate::new(48_000).unwrap());
        let mut output = Output::simulated(mixer);
        let tx = Weak::new();
        let format_a = Format {
            channels: ChannelCount::new(6).unwrap(),
            rate: SampleRate::new(44_100).unwrap(),
        };
        let format_b = Format {
            channels: ChannelCount::new(2).unwrap(),
            rate: SampleRate::new(48_000).unwrap(),
        };

        output.mixer(format_a, &tx).unwrap();
        assert!(!output.used_default_fallback());

        output.stream_failed(ErrorKind::UnsupportedConfig);
        output.mixer(format_a, &tx).unwrap();
        assert!(output.used_default_fallback());

        output.mixer(format_b, &tx).unwrap();
        assert!(!output.used_default_fallback());

        // First quick failure retries native configuration; second quick failure falls back to default.
        output.stream_failed(ErrorKind::StreamInvalidated);
        assert!(output.next_retry().is_some());
        output.mixer(format_b, &tx).unwrap();
        assert!(!output.used_default_fallback());

        output.stream_failed(ErrorKind::StreamInvalidated);
        assert!(output.next_retry().is_some());
        output.mixer(format_b, &tx).unwrap();
        assert!(output.used_default_fallback());
    }
}
