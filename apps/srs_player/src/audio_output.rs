use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

const MIN_RING_SAMPLES: usize = 4_096;
const MAX_RING_SAMPLES: usize = 1_048_576;
const TARGET_BUFFER_MS: u64 = 500;

#[derive(Debug, Clone, Copy)]
struct AudibleAnchor {
    playback_nanos: u128,
    consumed_samples_before_buffer: u64,
    timing_generation: u64,
}

#[derive(Debug, Default)]
struct AudibleAnchorSnapshot {
    sequence: AtomicU64,
    playback_nanos_hi: AtomicU64,
    playback_nanos_lo: AtomicU64,
    consumed_samples_before_buffer: AtomicU64,
    timing_generation: AtomicU64,
}

impl AudibleAnchorSnapshot {
    fn publish(&self, anchor: AudibleAnchor) {
        let odd = self.sequence.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        debug_assert_eq!(odd & 1, 1);
        self.playback_nanos_hi
            .store((anchor.playback_nanos >> 64) as u64, Ordering::SeqCst);
        self.playback_nanos_lo
            .store(anchor.playback_nanos as u64, Ordering::SeqCst);
        self.consumed_samples_before_buffer
            .store(anchor.consumed_samples_before_buffer, Ordering::SeqCst);
        self.timing_generation
            .store(anchor.timing_generation, Ordering::SeqCst);
        self.sequence.fetch_add(1, Ordering::SeqCst);
    }

    fn read(&self) -> Option<AudibleAnchor> {
        for _ in 0..3 {
            let first = self.sequence.load(Ordering::SeqCst);
            if first & 1 != 0 {
                continue;
            }
            let hi = self.playback_nanos_hi.load(Ordering::SeqCst);
            let lo = self.playback_nanos_lo.load(Ordering::SeqCst);
            let consumed_samples_before_buffer =
                self.consumed_samples_before_buffer.load(Ordering::SeqCst);
            let timing_generation = self.timing_generation.load(Ordering::SeqCst);
            let second = self.sequence.load(Ordering::SeqCst);
            if first == second && second & 1 == 0 && second != 0 {
                return Some(AudibleAnchor {
                    playback_nanos: (u128::from(hi) << 64) | u128::from(lo),
                    consumed_samples_before_buffer,
                    timing_generation,
                });
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AudioTelemetry {
    pub consumed_samples: u64,
    pub underrun_samples: u64,
    pub stream_errors: u64,
    pub requested_epoch: u64,
    pub callback_epoch: u64,
}

pub(crate) trait AudioSink: Send {
    fn matches_format(&self, sample_rate: u32, channels: u8) -> bool;
    fn request_epoch(&self, epoch: u64);
    fn epoch_ready(&self, epoch: u64) -> bool;
    fn push_pcm(&mut self, epoch: u64, samples: &[i16]) -> Result<usize>;
    fn telemetry(&self) -> AudioTelemetry;
    fn estimated_audible_samples(&self) -> Option<u64>;
    fn invalidate_audible_anchor(&self);
    fn buffered_samples(&self) -> usize;
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn pause(&self) -> Result<()>;
    fn play(&self) -> Result<()>;
}

pub struct AudioOutput {
    producer: Producer<i16>,
    stream: Stream,
    requested_epoch: Arc<AtomicU64>,
    callback_epoch: Arc<AtomicU64>,
    consumed_samples: Arc<AtomicU64>,
    underrun_samples: Arc<AtomicU64>,
    stream_errors: Arc<AtomicU64>,
    timing_generation: Arc<AtomicU64>,
    audible_anchor: Arc<AudibleAnchorSnapshot>,
    audible_floor_samples: AtomicU64,
    ring_capacity: usize,
    sample_rate: u32,
    channels: u16,
}

impl AudioOutput {
    pub fn open(sample_rate: u32, channels: u8, initial_epoch: u64) -> Result<Self> {
        if sample_rate == 0 {
            return Err(anyhow!("decoded audio sample rate must be non-zero"));
        }
        if channels == 0 {
            return Err(anyhow!("decoded audio channel count must be non-zero"));
        }

        let channels = u16::from(channels);
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device available"))?;

        let supported = select_exact_config(&device, sample_rate, channels)?;
        let sample_format = supported.sample_format();
        let stream_config = supported.config();

        let capacity = ring_capacity_samples(sample_rate, channels)?;
        let (producer, consumer) = RingBuffer::<i16>::new(capacity);

        let requested_epoch = Arc::new(AtomicU64::new(initial_epoch));
        let callback_epoch = Arc::new(AtomicU64::new(initial_epoch));
        let consumed_samples = Arc::new(AtomicU64::new(0));
        let underrun_samples = Arc::new(AtomicU64::new(0));
        let stream_errors = Arc::new(AtomicU64::new(0));
        let timing_generation = Arc::new(AtomicU64::new(1));
        let audible_anchor = Arc::new(AudibleAnchorSnapshot::default());

        let stream = build_stream_for_format(
            &device,
            stream_config,
            sample_format,
            consumer,
            Arc::clone(&requested_epoch),
            Arc::clone(&callback_epoch),
            Arc::clone(&consumed_samples),
            Arc::clone(&underrun_samples),
            Arc::clone(&stream_errors),
            Arc::clone(&timing_generation),
            Arc::clone(&audible_anchor),
        )?;
        stream
            .play()
            .context("failed to start audio output stream")?;

        Ok(Self {
            producer,
            stream,
            requested_epoch,
            callback_epoch,
            consumed_samples,
            underrun_samples,
            stream_errors,
            timing_generation,
            audible_anchor,
            audible_floor_samples: AtomicU64::new(0),
            ring_capacity: capacity,
            sample_rate,
            channels,
        })
    }

    pub fn matches_format(&self, sample_rate: u32, channels: u8) -> bool {
        self.sample_rate == sample_rate && self.channels == u16::from(channels)
    }

    pub fn request_epoch(&self, epoch: u64) {
        self.invalidate_audible_anchor();
        self.requested_epoch.store(epoch, Ordering::Release);
    }

    pub fn epoch_ready(&self, epoch: u64) -> bool {
        self.callback_epoch.load(Ordering::Acquire) == epoch
            && self.requested_epoch.load(Ordering::Acquire) == epoch
    }

    pub fn push_pcm(&mut self, epoch: u64, samples: &[i16]) -> Result<usize> {
        if !self.epoch_ready(epoch) {
            return Ok(0);
        }
        let (remaining_head, remaining_tail) = self.producer.push_partial_slice(samples);
        Ok(samples
            .len()
            .saturating_sub(remaining_head.len().saturating_add(remaining_tail.len())))
    }

    pub fn available_slots(&self) -> usize {
        self.producer.slots()
    }

    pub fn buffered_samples(&self) -> usize {
        self.ring_capacity.saturating_sub(self.producer.slots())
    }

    pub fn telemetry(&self) -> AudioTelemetry {
        AudioTelemetry {
            consumed_samples: self.consumed_samples.load(Ordering::Relaxed),
            underrun_samples: self.underrun_samples.load(Ordering::Relaxed),
            stream_errors: self.stream_errors.load(Ordering::Relaxed),
            requested_epoch: self.requested_epoch.load(Ordering::Acquire),
            callback_epoch: self.callback_epoch.load(Ordering::Acquire),
        }
    }

    pub fn invalidate_audible_anchor(&self) {
        self.audible_floor_samples.store(
            self.consumed_samples.load(Ordering::Acquire),
            Ordering::Release,
        );
        self.timing_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn estimated_audible_samples(&self) -> Option<u64> {
        let anchor = self.audible_anchor.read()?;
        let generation = self.timing_generation.load(Ordering::Acquire);
        if anchor.timing_generation != generation {
            return None;
        }

        let secs = u64::try_from(anchor.playback_nanos / 1_000_000_000).ok()?;
        let nanos = (anchor.playback_nanos % 1_000_000_000) as u32;
        let playback = cpal::StreamInstant::new(secs, nanos);
        let estimate = estimate_audible_samples(
            anchor.consumed_samples_before_buffer,
            playback,
            self.stream.now(),
            self.consumed_samples.load(Ordering::Acquire),
            self.sample_rate,
            self.channels,
        )?;
        let previous = self
            .audible_floor_samples
            .fetch_max(estimate, Ordering::AcqRel);
        Some(previous.max(estimate))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn pause(&self) -> Result<()> {
        self.invalidate_audible_anchor();
        self.stream.pause().context("failed to pause audio stream")
    }

    pub fn play(&self) -> Result<()> {
        self.stream.play().context("failed to resume audio stream")
    }
}

impl AudioSink for AudioOutput {
    fn matches_format(&self, sample_rate: u32, channels: u8) -> bool {
        AudioOutput::matches_format(self, sample_rate, channels)
    }

    fn request_epoch(&self, epoch: u64) {
        AudioOutput::request_epoch(self, epoch);
    }

    fn epoch_ready(&self, epoch: u64) -> bool {
        AudioOutput::epoch_ready(self, epoch)
    }

    fn push_pcm(&mut self, epoch: u64, samples: &[i16]) -> Result<usize> {
        AudioOutput::push_pcm(self, epoch, samples)
    }

    fn telemetry(&self) -> AudioTelemetry {
        AudioOutput::telemetry(self)
    }

    fn estimated_audible_samples(&self) -> Option<u64> {
        AudioOutput::estimated_audible_samples(self)
    }

    fn invalidate_audible_anchor(&self) {
        AudioOutput::invalidate_audible_anchor(self);
    }

    fn buffered_samples(&self) -> usize {
        AudioOutput::buffered_samples(self)
    }

    fn sample_rate(&self) -> u32 {
        AudioOutput::sample_rate(self)
    }

    fn channels(&self) -> u16 {
        AudioOutput::channels(self)
    }

    fn pause(&self) -> Result<()> {
        AudioOutput::pause(self)
    }

    fn play(&self) -> Result<()> {
        AudioOutput::play(self)
    }
}

fn estimate_audible_samples(
    consumed_samples_before_buffer: u64,
    playback: cpal::StreamInstant,
    now: cpal::StreamInstant,
    consumed_samples: u64,
    sample_rate: u32,
    channels: u16,
) -> Option<u64> {
    let elapsed = now.checked_duration_since(playback)?;
    let samples_per_second = u64::from(sample_rate).checked_mul(u64::from(channels))?;
    if samples_per_second == 0 {
        return None;
    }
    let elapsed_samples = u64::try_from(
        elapsed
            .as_nanos()
            .saturating_mul(u128::from(samples_per_second))
            / 1_000_000_000,
    )
    .ok()?;
    let candidate = consumed_samples_before_buffer.saturating_add(elapsed_samples);
    Some(candidate.min(consumed_samples))
}

fn ring_capacity_samples(sample_rate: u32, channels: u16) -> Result<usize> {
    let samples = u64::from(sample_rate)
        .checked_mul(u64::from(channels))
        .and_then(|value| value.checked_mul(TARGET_BUFFER_MS))
        .map(|value| value / 1_000)
        .ok_or_else(|| anyhow!("audio ring capacity overflow"))?;
    let samples = usize::try_from(samples).context("audio ring capacity does not fit usize")?;
    Ok(samples.clamp(MIN_RING_SAMPLES, MAX_RING_SAMPLES))
}

fn select_exact_config(
    device: &cpal::Device,
    sample_rate: u32,
    channels: u16,
) -> Result<SupportedStreamConfig> {
    let mut candidates = device
        .supported_output_configs()
        .context("failed to enumerate supported audio output configurations")?
        .filter(|range| {
            range.channels() == channels && is_supported_pcm_format(range.sample_format())
        })
        .filter_map(|range| range.try_with_sample_rate(sample_rate))
        .collect::<Vec<_>>();

    candidates.sort_by_key(|config| sample_format_rank(config.sample_format()));
    candidates
        .pop()
        .ok_or_else(|| anyhow!(
            "audio device has no supported PCM configuration for {sample_rate} Hz / {channels} channels"
        ))
}

fn is_supported_pcm_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::F32
            | SampleFormat::F64
            | SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U32
            | SampleFormat::U64
    )
}

fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 10,
        SampleFormat::F64 => 9,
        SampleFormat::I32 => 8,
        SampleFormat::I16 => 7,
        SampleFormat::U32 => 6,
        SampleFormat::U16 => 5,
        SampleFormat::I8 => 4,
        SampleFormat::U8 => 3,
        SampleFormat::I64 => 2,
        SampleFormat::U64 => 1,
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_stream_for_format(
    device: &cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    consumer: Consumer<i16>,
    requested_epoch: Arc<AtomicU64>,
    callback_epoch: Arc<AtomicU64>,
    consumed_samples: Arc<AtomicU64>,
    underrun_samples: Arc<AtomicU64>,
    stream_errors: Arc<AtomicU64>,
    timing_generation: Arc<AtomicU64>,
    audible_anchor: Arc<AudibleAnchorSnapshot>,
) -> Result<Stream> {
    let channels = usize::from(config.channels);
    macro_rules! build {
        ($sample:ty) => {{
            build_typed_stream::<$sample>(
                device,
                config,
                channels,
                consumer,
                requested_epoch,
                callback_epoch,
                consumed_samples,
                underrun_samples,
                stream_errors,
                timing_generation,
                audible_anchor,
            )
        }};
    }

    match sample_format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        other => Err(anyhow!("unsupported PCM device sample format: {other}")),
    }
}

trait FromPcmI16: SizedSample {
    fn from_pcm_i16(sample: i16) -> Self;
    fn silence() -> Self;
}

macro_rules! impl_signed_pcm {
    ($ty:ty, $shift:expr) => {
        impl FromPcmI16 for $ty {
            fn from_pcm_i16(sample: i16) -> Self {
                (sample as $ty) << $shift
            }
            fn silence() -> Self {
                0
            }
        }
    };
}

impl FromPcmI16 for i16 {
    fn from_pcm_i16(sample: i16) -> Self {
        sample
    }
    fn silence() -> Self {
        0
    }
}
impl_signed_pcm!(i32, 16);
impl_signed_pcm!(i64, 48);

impl FromPcmI16 for i8 {
    fn from_pcm_i16(sample: i16) -> Self {
        (sample >> 8) as i8
    }
    fn silence() -> Self {
        0
    }
}

impl FromPcmI16 for f32 {
    fn from_pcm_i16(sample: i16) -> Self {
        f32::from(sample) / 32_768.0
    }
    fn silence() -> Self {
        0.0
    }
}

impl FromPcmI16 for f64 {
    fn from_pcm_i16(sample: i16) -> Self {
        f64::from(sample) / 32_768.0
    }
    fn silence() -> Self {
        0.0
    }
}

impl FromPcmI16 for u8 {
    fn from_pcm_i16(sample: i16) -> Self {
        ((i32::from(sample) + 32_768) >> 8) as u8
    }
    fn silence() -> Self {
        128
    }
}

impl FromPcmI16 for u16 {
    fn from_pcm_i16(sample: i16) -> Self {
        (i32::from(sample) + 32_768) as u16
    }
    fn silence() -> Self {
        32_768
    }
}

impl FromPcmI16 for u32 {
    fn from_pcm_i16(sample: i16) -> Self {
        ((i64::from(sample) + 32_768) as u32) << 16
    }
    fn silence() -> Self {
        1_u32 << 31
    }
}

impl FromPcmI16 for u64 {
    fn from_pcm_i16(sample: i16) -> Self {
        ((i128::from(sample) + 32_768) as u64) << 48
    }
    fn silence() -> Self {
        1_u64 << 63
    }
}

#[allow(clippy::too_many_arguments)]
fn build_typed_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    channels: usize,
    mut consumer: Consumer<i16>,
    requested_epoch: Arc<AtomicU64>,
    callback_epoch: Arc<AtomicU64>,
    consumed_samples: Arc<AtomicU64>,
    underrun_samples: Arc<AtomicU64>,
    stream_errors: Arc<AtomicU64>,
    timing_generation: Arc<AtomicU64>,
    audible_anchor: Arc<AudibleAnchorSnapshot>,
) -> Result<Stream>
where
    T: FromPcmI16 + Send + 'static,
{
    let callback_stream_errors = Arc::clone(&stream_errors);
    device
        .build_output_stream(
            config,
            move |output: &mut [T], info| {
                let callback_timing_generation = timing_generation.load(Ordering::Acquire);
                let requested = requested_epoch.load(Ordering::Acquire);
                let active = callback_epoch.load(Ordering::Acquire);
                if requested != active {
                    let available = consumer.slots();
                    if available != 0 {
                        if let Ok(chunk) = consumer.read_chunk(available) {
                            chunk.commit_all();
                        }
                    }
                    callback_epoch.store(requested, Ordering::Release);
                    fill_silence(output);
                    return;
                }

                let available = consumer.slots().min(output.len());
                let available = if channels == 0 {
                    0
                } else {
                    available - (available % channels)
                };
                let consumed_before = consumed_samples.load(Ordering::Relaxed);
                let consumed = if available == 0 {
                    0
                } else {
                    match consumer.read_chunk(available) {
                        Ok(chunk) => {
                            let (head, tail) = chunk.as_slices();
                            let head_len = head.len();
                            copy_pcm_to_output(head, &mut output[..head_len]);
                            copy_pcm_to_output(tail, &mut output[head_len..head_len + tail.len()]);
                            chunk.commit_all();
                            available
                        }
                        Err(_) => 0,
                    }
                };

                if consumed < output.len() {
                    fill_silence(&mut output[consumed..]);
                }
                if consumed != 0 {
                    consumed_samples.fetch_add(consumed as u64, Ordering::Relaxed);
                    let playback = info.timestamp().playback.as_nanos();
                    audible_anchor.publish(AudibleAnchor {
                        playback_nanos: playback,
                        consumed_samples_before_buffer: consumed_before,
                        timing_generation: callback_timing_generation,
                    });
                }
                let underrun = output.len().saturating_sub(consumed);
                if underrun != 0 {
                    underrun_samples.fetch_add(underrun as u64, Ordering::Relaxed);
                }
            },
            move |_error| {
                callback_stream_errors.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .context("failed to build exact-format audio output stream")
}

fn copy_pcm_to_output<T: FromPcmI16>(input: &[i16], output: &mut [T]) {
    debug_assert_eq!(input.len(), output.len());
    for (source, destination) in input.iter().zip(output.iter_mut()) {
        *destination = T::from_pcm_i16(*source);
    }
}

fn fill_silence<T: FromPcmI16>(output: &mut [T]) {
    for sample in output.iter_mut() {
        *sample = T::silence();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_consumption_alignment_preserves_channel_frames() {
        fn aligned(available: usize, channels: usize) -> usize {
            if channels == 0 {
                0
            } else {
                available - (available % channels)
            }
        }

        assert_eq!(aligned(5, 2), 4);
        assert_eq!(aligned(8, 2), 8);
        assert_eq!(aligned(10, 6), 6);
        assert_eq!(aligned(2, 6), 0);
    }

    #[test]
    fn audible_anchor_snapshot_preserves_full_u128_stream_time() {
        let snapshot = AudibleAnchorSnapshot::default();
        let playback_nanos = (u128::from(u64::MAX) << 64) | 0x1234_5678_9abc_def0;
        snapshot.publish(AudibleAnchor {
            playback_nanos,
            consumed_samples_before_buffer: 987_654,
            timing_generation: 42,
        });

        let anchor = snapshot.read().expect("coherent anchor");
        assert_eq!(anchor.playback_nanos, playback_nanos);
        assert_eq!(anchor.consumed_samples_before_buffer, 987_654);
        assert_eq!(anchor.timing_generation, 42);
    }

    #[test]
    fn audible_estimate_advances_with_stream_time_but_caps_at_real_pcm() {
        let playback = cpal::StreamInstant::new(10, 0);
        let now = cpal::StreamInstant::new(10, 500_000_000);
        assert_eq!(
            estimate_audible_samples(96_000, playback, now, 200_000, 48_000, 2),
            Some(144_000)
        );
        assert_eq!(
            estimate_audible_samples(96_000, playback, now, 120_000, 48_000, 2),
            Some(120_000)
        );
    }

    #[test]
    fn audible_estimate_candidate_can_be_clamped_monotonically() {
        let floor = AtomicU64::new(90_000);
        let estimate = 80_000;
        let previous = floor.fetch_max(estimate, Ordering::AcqRel);
        assert_eq!(previous.max(estimate), 90_000);

        let estimate = 100_000;
        let previous = floor.fetch_max(estimate, Ordering::AcqRel);
        assert_eq!(previous.max(estimate), 100_000);
    }

    #[test]
    fn audible_estimate_rejects_regressing_stream_time() {
        let playback = cpal::StreamInstant::new(11, 0);
        let now = cpal::StreamInstant::new(10, 999_999_999);
        assert_eq!(
            estimate_audible_samples(0, playback, now, 1_000, 48_000, 2),
            None
        );
    }

    #[test]
    fn capacity_is_bounded() {
        assert_eq!(
            ring_capacity_samples(8_000, 1).expect("capacity"),
            MIN_RING_SAMPLES
        );
        assert_eq!(
            ring_capacity_samples(768_000, 64).expect("capacity"),
            MAX_RING_SAMPLES
        );
    }

    #[test]
    fn integer_conversions_preserve_silence_and_extremes() {
        assert_eq!(i16::from_pcm_i16(i16::MIN), i16::MIN);
        assert_eq!(u16::from_pcm_i16(i16::MIN), 0);
        assert_eq!(u16::from_pcm_i16(0), 32_768);
        assert_eq!(u16::from_pcm_i16(i16::MAX), u16::MAX);
        assert_eq!(i32::from_pcm_i16(i16::MIN), i32::MIN);
        assert_eq!(u32::from_pcm_i16(i16::MIN), 0);
    }

    #[test]
    fn float_conversions_are_normalized() {
        assert_eq!(f32::from_pcm_i16(0), 0.0);
        assert_eq!(f64::from_pcm_i16(0), 0.0);
        assert!(f32::from_pcm_i16(i16::MAX) < 1.0);
        assert_eq!(f32::from_pcm_i16(i16::MIN), -1.0);
    }
}
