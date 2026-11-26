use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

fn duration_to_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u64::MAX as u128) as u64
}

/// Playback position projected to UI and MPRIS without locking the PCM buffer.
#[derive(Debug)]
pub struct AudioClock {
    pos_us: AtomicU64,
}

impl AudioClock {
    pub fn new(initial: Duration) -> Self {
        Self {
            pos_us: AtomicU64::new(duration_to_micros(initial)),
        }
    }

    pub fn store(&self, pos: Duration) {
        self.pos_us.store(duration_to_micros(pos), Ordering::Relaxed);
    }

    pub fn position(&self) -> Duration {
        Duration::from_micros(self.pos_us.load(Ordering::Relaxed))
    }
}
