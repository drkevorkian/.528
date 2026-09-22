use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::audio_output::{AudioOutput, AudioSink, AudioTelemetry};
use libsrs_app_services::{
    DecodedAudioChunk, DecodedVideoFrame, PlaybackEvent, PlaybackSession, PlaybackState,
};

const COMMAND_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 16;
const PLAYBACK_TICK: Duration = Duration::from_millis(33);
const MAX_PRESENTATION_REORDER_FRAMES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerState {
    Closed,
    Opening,
    Ready,
    Playing,
    Paused,
    Seeking,
    Ended,
    Error,
}

#[derive(Debug, Clone)]
pub struct PlaybackSnapshot {
    pub generation: u64,
    pub state: PlayerState,
    pub duration_ms: u64,
    pub presented_position_ms: u64,
    pub decoded_position_ms: u64,
    pub decoded_video_frames: u64,
    pub decoded_audio_chunks: u64,
    pub presented_video_frames: u64,
    pub dropped_video_frames: u64,
    pub reorder_depth: usize,
    pub seek_in_progress: bool,
    pub audio_media_position_ms: Option<u64>,
    pub audio_consumed_samples: u64,
    pub audio_underrun_samples: u64,
    pub audio_stream_errors: u64,
    pub last_error: Option<String>,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            generation: 0,
            state: PlayerState::Closed,
            duration_ms: 0,
            presented_position_ms: 0,
            decoded_position_ms: 0,
            decoded_video_frames: 0,
            decoded_audio_chunks: 0,
            presented_video_frames: 0,
            dropped_video_frames: 0,
            reorder_depth: 0,
            seek_in_progress: false,
            audio_media_position_ms: None,
            audio_consumed_samples: 0,
            audio_underrun_samples: 0,
            audio_stream_errors: 0,
            last_error: None,
        }
    }
}

#[derive(Debug)]
pub struct PresentationFrame {
    pub generation: u64,
    pub frame: DecodedVideoFrame,
    pub presented_position_ms: u64,
}

#[derive(Debug)]
pub enum PlaybackWorkerCommand {
    Open { generation: u64, path: PathBuf },
    Play { generation: u64 },
    Pause { generation: u64 },
    Stop { generation: u64 },
    Close { generation: u64 },
    Seek { generation: u64, target_ms: u64 },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum PlaybackWorkerEvent {
    FrameReady {
        generation: u64,
    },
    SeekCompleted {
        generation: u64,
        presented_position_ms: u64,
    },
    FatalError {
        generation: u64,
        message: String,
    },
    WorkerStopped,
}

pub struct PlaybackWorkerHandle {
    command_tx: SyncSender<PlaybackWorkerCommand>,
    event_rx: Receiver<PlaybackWorkerEvent>,
    frame_slot: Arc<Mutex<Option<PresentationFrame>>>,
    snapshot_slot: Arc<Mutex<PlaybackSnapshot>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PlaybackWorkerHandle {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let frame_slot = Arc::new(Mutex::new(None));
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let worker_slot = Arc::clone(&frame_slot);
        let worker_snapshot_slot = Arc::clone(&snapshot_slot);
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = thread::Builder::new()
            .name("srs-playback-worker".to_string())
            .spawn(move || {
                let mut worker = PlaybackWorker::new(
                    command_rx,
                    event_tx,
                    worker_slot,
                    worker_snapshot_slot,
                    worker_shutdown,
                );
                worker.run();
            })
            .ok();

        Self {
            command_tx,
            event_rx,
            frame_slot,
            snapshot_slot,
            shutdown,
            thread,
        }
    }

    pub fn try_send(
        &self,
        command: PlaybackWorkerCommand,
    ) -> Result<(), mpsc::TrySendError<PlaybackWorkerCommand>> {
        self.command_tx.try_send(command)
    }

    pub fn try_recv_event(&self) -> Result<PlaybackWorkerEvent, TryRecvError> {
        self.event_rx.try_recv()
    }

    pub fn latest_snapshot(&self) -> Result<PlaybackSnapshot, String> {
        match self.snapshot_slot.lock() {
            Ok(snapshot) => Ok(snapshot.clone()),
            Err(poisoned) => {
                drop(poisoned.into_inner());
                Err("playback snapshot mutex was poisoned".to_string())
            }
        }
    }

    pub fn take_latest_frame(&self) -> Result<Option<PresentationFrame>, String> {
        match self.frame_slot.lock() {
            Ok(mut slot) => Ok(slot.take()),
            Err(poisoned) => {
                let mut slot = poisoned.into_inner();
                slot.take();
                Err("playback frame slot mutex was poisoned".to_string())
            }
        }
    }
}

impl Drop for PlaybackWorkerHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(PlaybackWorkerCommand::Shutdown);
        // Do not block the egui thread waiting for a decoder/file operation to finish. Dropping
        // the JoinHandle detaches the worker; the atomic shutdown flag is checked between steps.
        let _ = self.thread.take();
    }
}

struct PresentationReorder {
    next_display_index: Option<u32>,
    pending: BTreeMap<u32, DecodedVideoFrame>,
}

impl PresentationReorder {
    fn new() -> Self {
        Self {
            next_display_index: None,
            pending: BTreeMap::new(),
        }
    }

    fn reset(&mut self, presentation_floor: Option<u32>) {
        self.pending.clear();
        self.next_display_index = presentation_floor;
    }

    fn depth(&self) -> usize {
        self.pending.len()
    }

    fn push(&mut self, frame: DecodedVideoFrame) -> Result<bool, String> {
        let index = frame.frame_index;
        let next = self.next_display_index.get_or_insert(index);

        if index < *next {
            return Ok(false);
        }
        let gap = index.saturating_sub(*next) as usize;
        if gap > MAX_PRESENTATION_REORDER_FRAMES {
            return Err(format!(
                "presentation index gap {gap} exceeds reorder bound {MAX_PRESENTATION_REORDER_FRAMES}"
            ));
        }
        if self.pending.insert(index, frame).is_some() {
            return Err(format!("duplicate presentation frame index {index}"));
        }
        if self.pending.len() > MAX_PRESENTATION_REORDER_FRAMES + 1 {
            return Err(format!(
                "presentation reorder depth {} exceeds bound",
                self.pending.len()
            ));
        }
        Ok(true)
    }

    fn pop_ready(&mut self) -> Option<DecodedVideoFrame> {
        let next = self.next_display_index?;
        let frame = self.pending.remove(&next)?;
        self.next_display_index = Some(next.saturating_add(1));
        Some(frame)
    }

    fn finish_eos(&self) -> Result<(), String> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            let expected = self.next_display_index.unwrap_or(0);
            let first = self.pending.keys().next().copied().unwrap_or(expected);
            Err(format!(
                "end of stream with unresolved presentation gap: expected {expected}, first buffered {first}"
            ))
        }
    }
}

struct PendingAudioChunk {
    sample_rate: u32,
    channels: u8,
    samples: Vec<i16>,
    offset: usize,
}

struct PlaybackWorker {
    command_rx: Receiver<PlaybackWorkerCommand>,
    event_tx: SyncSender<PlaybackWorkerEvent>,
    frame_slot: Arc<Mutex<Option<PresentationFrame>>>,
    snapshot_slot: Arc<Mutex<PlaybackSnapshot>>,
    shutdown: Arc<AtomicBool>,
    session: Option<PlaybackSession>,
    generation: u64,
    state: PlayerState,
    reorder: PresentationReorder,
    presented_video_frames: u64,
    dropped_video_frames: u64,
    presented_position_ms: u64,
    presentation_time_slots_ms: VecDeque<u64>,
    audio_output: Option<Box<dyn AudioSink>>,
    pending_audio: Option<PendingAudioChunk>,
    audio_epoch: u64,
    audio_epoch_media_start_ms: Option<u64>,
    audio_epoch_consumed_base: u64,
    audio_epoch_armed: bool,
    audio_last_stream_errors: u64,
}

impl PlaybackWorker {
    fn new(
        command_rx: Receiver<PlaybackWorkerCommand>,
        event_tx: SyncSender<PlaybackWorkerEvent>,
        frame_slot: Arc<Mutex<Option<PresentationFrame>>>,
        snapshot_slot: Arc<Mutex<PlaybackSnapshot>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            command_rx,
            event_tx,
            frame_slot,
            snapshot_slot,
            shutdown,
            session: None,
            generation: 0,
            state: PlayerState::Closed,
            reorder: PresentationReorder::new(),
            presented_video_frames: 0,
            dropped_video_frames: 0,
            presented_position_ms: 0,
            presentation_time_slots_ms: VecDeque::with_capacity(
                MAX_PRESENTATION_REORDER_FRAMES + 1,
            ),
            audio_output: None,
            pending_audio: None,
            audio_epoch: 1,
            audio_epoch_media_start_ms: None,
            audio_epoch_consumed_base: 0,
            audio_epoch_armed: false,
            audio_last_stream_errors: 0,
        }
    }

    fn run(&mut self) {
        while !self.shutdown.load(Ordering::Acquire) {
            if self.state == PlayerState::Playing {
                match self.command_rx.recv_timeout(PLAYBACK_TICK) {
                    Ok(command) => {
                        if !self.handle_command(command) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => self.playback_step(),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match self.command_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(command) => {
                        if !self.handle_command(command) {
                            break;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        self.session = None;
        let _ = self.event_tx.try_send(PlaybackWorkerEvent::WorkerStopped);
    }

    fn command_generation_is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    fn handle_command(&mut self, command: PlaybackWorkerCommand) -> bool {
        match command {
            PlaybackWorkerCommand::Shutdown => return false,
            PlaybackWorkerCommand::Open { generation, path } => {
                self.generation = generation;
                // Fail closed on reopen: drop the previous media/session before touching the new
                // path so an Open failure cannot leave old content playable under a new generation.
                self.session = None;
                self.reset_audio_for_open();
                self.reorder.reset(None);
                self.presentation_time_slots_ms.clear();
                self.clear_frame_slot();
                self.presented_video_frames = 0;
                self.dropped_video_frames = 0;
                self.presented_position_ms = 0;
                self.state = PlayerState::Opening;
                self.emit_snapshot();
                match PlaybackSession::open(&path) {
                    Ok(session) => {
                        self.session = Some(session);
                        self.state = PlayerState::Ready;
                        self.emit_snapshot();
                    }
                    Err(error) => self.fail(error.to_string()),
                }
            }
            PlaybackWorkerCommand::Play { generation } => {
                if !self.command_generation_is_current(generation) {
                    return true;
                }
                let Some(state) = self.session.as_ref().map(PlaybackSession::state) else {
                    self.fail("playback session is not open".to_string());
                    return true;
                };
                if state == PlaybackState::EndOfStream {
                    if let Some(session) = self.session.as_mut() {
                        if let Err(error) = session.stop() {
                            self.fail(error.to_string());
                            return true;
                        }
                    }
                    self.reorder.reset(None);
                    self.presentation_time_slots_ms.clear();
                    self.advance_audio_epoch(Some(0));
                }
                if let Some(audio) = self.audio_output.as_ref() {
                    if let Err(error) = audio.play() {
                        self.fail(error.to_string());
                        return true;
                    }
                }
                if let Some(session) = self.session.as_mut() {
                    session.play();
                }
                self.state = PlayerState::Playing;
                self.emit_snapshot();
            }
            PlaybackWorkerCommand::Pause { generation } => {
                if !self.command_generation_is_current(generation) {
                    return true;
                }
                if let Some(session) = self.session.as_mut() {
                    session.pause();
                }
                if let Some(audio) = self.audio_output.as_ref() {
                    if let Err(error) = audio.pause() {
                        self.fail(error.to_string());
                        return true;
                    }
                }
                self.state = PlayerState::Paused;
                self.emit_snapshot();
            }
            PlaybackWorkerCommand::Stop { generation } => {
                if !self.command_generation_is_current(generation) {
                    return true;
                }
                if let Some(session) = self.session.as_mut() {
                    if let Err(error) = session.stop() {
                        self.fail(error.to_string());
                        return true;
                    }
                }
                self.reorder.reset(None);
                self.presentation_time_slots_ms.clear();
                self.clear_frame_slot();
                self.presented_position_ms = 0;
                self.advance_audio_epoch(Some(0));
                if let Some(audio) = self.audio_output.as_ref() {
                    if let Err(error) = audio.pause() {
                        self.fail(error.to_string());
                        return true;
                    }
                }
                self.state = PlayerState::Ready;
                self.emit_snapshot();
            }
            PlaybackWorkerCommand::Close { generation } => {
                self.generation = generation;
                self.session = None;
                self.reset_audio_for_open();
                self.reorder.reset(None);
                self.presentation_time_slots_ms.clear();
                self.clear_frame_slot();
                self.presented_position_ms = 0;
                self.state = PlayerState::Closed;
                self.emit_snapshot();
            }
            PlaybackWorkerCommand::Seek {
                generation,
                target_ms,
            } => {
                if generation != self.generation {
                    self.generation = generation;
                }
                self.perform_seek(target_ms);
            }
        }
        true
    }

    fn playback_step(&mut self) {
        match self.flush_pending_audio() {
            Ok(true) => {}
            Ok(false) => {
                self.emit_snapshot();
                return;
            }
            Err(error) => {
                self.fail(error);
                return;
            }
        }

        if let Some(frame) = self.reorder.pop_ready() {
            let Some(position_ms) = self.presentation_time_slots_ms.pop_front() else {
                self.fail("presentation frame became ready without a timestamp slot".to_string());
                return;
            };
            self.publish_frame_at_position(frame, position_ms);
            return;
        }

        let event = {
            let Some(session) = self.session.as_mut() else {
                self.fail("playback session disappeared".to_string());
                return;
            };
            session.decode_next_step()
        };

        match event {
            Ok(PlaybackEvent::Video(frame)) => {
                let slot_ms = frame_position_ms(&frame);
                let accepted = match self.reorder.push(frame) {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        self.fail(error);
                        return;
                    }
                };
                if !accepted {
                    self.fail(
                        "displayable frame arrived after its presentation index was already passed"
                            .to_string(),
                    );
                    return;
                }
                if let Err(error) = self.push_time_slot_ms(slot_ms) {
                    self.fail(error);
                    return;
                }
                if let Some(frame) = self.reorder.pop_ready() {
                    let Some(position_ms) = self.presentation_time_slots_ms.pop_front() else {
                        self.fail(
                            "presentation frame became ready without a timestamp slot".to_string(),
                        );
                        return;
                    };
                    self.publish_frame_at_position(frame, position_ms);
                }
                self.emit_snapshot();
            }
            Ok(PlaybackEvent::Audio(chunk)) => {
                if let Err(error) = self.queue_audio_chunk(chunk) {
                    self.fail(error);
                    return;
                }
                self.emit_snapshot();
            }
            Ok(PlaybackEvent::EndOfStream) => {
                if let Some(frame) = self.reorder.pop_ready() {
                    let Some(position_ms) = self.presentation_time_slots_ms.pop_front() else {
                        self.fail("EOS presentation frame missing timestamp slot".to_string());
                        return;
                    };
                    self.publish_frame_at_position(frame, position_ms);
                    return;
                }
                match self.reorder.finish_eos() {
                    Ok(()) if self.presentation_time_slots_ms.is_empty() => {
                        self.state = PlayerState::Ended;
                        self.emit_snapshot();
                    }
                    Ok(()) => self.fail(
                        "end of stream left unmatched presentation timestamp slots".to_string(),
                    ),
                    Err(error) => self.fail(error),
                }
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn perform_seek(&mut self, target_ms: u64) {
        let resume_playing = self.state == PlayerState::Playing;
        self.advance_audio_epoch(Some(target_ms));
        self.state = PlayerState::Seeking;
        self.clear_frame_slot();
        self.emit_snapshot();

        let srsv2_reordered = self
            .session
            .as_ref()
            .and_then(|session| session.primary_video())
            .is_some_and(|track| track.codec_id == 3);

        if !srsv2_reordered {
            self.reorder.reset(None);
            self.presentation_time_slots_ms.clear();
            let Some(session) = self.session.as_mut() else {
                self.fail("seek requested without an open playback session".to_string());
                return;
            };
            if let Err(error) = session.seek_ms(target_ms) {
                self.fail(error.to_string());
                return;
            }
            self.presented_position_ms = target_ms.min(session.duration_ms());
            self.state = if resume_playing {
                PlayerState::Playing
            } else {
                PlayerState::Paused
            };
            let _ = self.event_tx.try_send(PlaybackWorkerEvent::SeekCompleted {
                generation: self.generation,
                presented_position_ms: self.presented_position_ms,
            });
            self.emit_snapshot();
            return;
        }

        // Current native SRSV2 B-frame streams use frame_index as display identity, while packet
        // PTS advances in file/decode order. Recover from the previous keyframe, reorder by
        // frame_index, and pair the observed decode-order timestamp slots with display-ready
        // frames. This avoids inventing a fixed frame cadence.
        self.reorder.reset(None);
        self.presentation_time_slots_ms.clear();

        let Some(session) = self.session.as_mut() else {
            self.fail("seek requested without an open playback session".to_string());
            return;
        };

        let saved_video = session.decoded_video_frames;
        let saved_audio = session.decoded_audio_chunks;
        if let Err(error) = session.seek_video_keyframe_before_or_at_ms(target_ms) {
            self.fail(error.to_string());
            return;
        }

        const RECOVERY_STEP_BUDGET: usize = 8192;
        let mut selected: Option<(DecodedVideoFrame, u64)> = None;

        for _ in 0..RECOVERY_STEP_BUDGET {
            if self.shutdown.load(Ordering::Acquire) {
                break;
            }

            match session.decode_next_step() {
                Ok(PlaybackEvent::Video(frame)) => {
                    let slot_ms = frame_position_ms(&frame);
                    let accepted = match self.reorder.push(frame) {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            session.decoded_video_frames = saved_video;
                            session.decoded_audio_chunks = saved_audio;
                            self.fail(error);
                            return;
                        }
                    };
                    if !accepted {
                        session.decoded_video_frames = saved_video;
                        session.decoded_audio_chunks = saved_audio;
                        self.fail(
                            "seek recovery saw a display frame older than the presentation cursor"
                                .to_string(),
                        );
                        return;
                    }
                    if let Err(error) =
                        push_bounded_time_slot_ms(&mut self.presentation_time_slots_ms, slot_ms)
                    {
                        session.decoded_video_frames = saved_video;
                        session.decoded_audio_chunks = saved_audio;
                        self.fail(error);
                        return;
                    }

                    while let Some(frame) = self.reorder.pop_ready() {
                        let Some(position_ms) = self.presentation_time_slots_ms.pop_front() else {
                            session.decoded_video_frames = saved_video;
                            session.decoded_audio_chunks = saved_audio;
                            self.fail(
                                "seek recovery produced frame without timestamp slot".to_string(),
                            );
                            return;
                        };
                        if position_ms >= target_ms {
                            selected = Some((frame, position_ms));
                            break;
                        }
                    }

                    if selected.is_some() {
                        break;
                    }
                }
                Ok(PlaybackEvent::Audio(_)) => {}
                Ok(PlaybackEvent::EndOfStream) => break,
                Err(error) => {
                    session.decoded_video_frames = saved_video;
                    session.decoded_audio_chunks = saved_audio;
                    self.fail(error.to_string());
                    return;
                }
            }
        }

        session.decoded_video_frames = saved_video;
        session.decoded_audio_chunks = saved_audio;

        let Some((frame, position_ms)) = selected else {
            self.fail("seek recovery did not reach requested presentation time".to_string());
            return;
        };

        self.publish_frame_at_position(frame, position_ms);
        self.state = if resume_playing {
            PlayerState::Playing
        } else {
            PlayerState::Paused
        };
        let _ = self.event_tx.try_send(PlaybackWorkerEvent::SeekCompleted {
            generation: self.generation,
            presented_position_ms: self.presented_position_ms,
        });
        self.emit_snapshot();
    }

    fn reset_audio_for_open(&mut self) {
        self.audio_output = None;
        self.pending_audio = None;
        self.audio_epoch = self.audio_epoch.wrapping_add(1);
        self.audio_epoch_media_start_ms = None;
        self.audio_epoch_consumed_base = 0;
        self.audio_epoch_armed = false;
        self.audio_last_stream_errors = 0;
    }

    fn advance_audio_epoch(&mut self, media_start_ms: Option<u64>) {
        self.pending_audio = None;
        self.audio_epoch = self.audio_epoch.wrapping_add(1);
        self.audio_epoch_media_start_ms = media_start_ms;
        self.audio_epoch_consumed_base = 0;
        self.audio_epoch_armed = false;
        if let Some(audio) = self.audio_output.as_ref() {
            audio.request_epoch(self.audio_epoch);
        }
    }

    fn queue_audio_chunk(&mut self, chunk: DecodedAudioChunk) -> Result<(), String> {
        if self.pending_audio.is_some() {
            return Err("audio chunk arrived while previous PCM is still pending".to_string());
        }

        if self.audio_output.is_none() {
            let output = AudioOutput::open(chunk.sample_rate, chunk.channels, self.audio_epoch)
                .map_err(|error| format!("audio output initialization failed: {error:#}"))?;
            self.audio_last_stream_errors = output.telemetry().stream_errors;
            self.audio_output = Some(Box::new(output));
        }

        let Some(audio) = self.audio_output.as_ref() else {
            return Err("audio output disappeared after initialization".to_string());
        };
        if !audio.matches_format(chunk.sample_rate, chunk.channels) {
            return Err(format!(
                "decoded audio format changed from {} Hz / {} channels to {} Hz / {} channels",
                audio.sample_rate(),
                audio.channels(),
                chunk.sample_rate,
                chunk.channels
            ));
        }

        if self.audio_epoch_media_start_ms.is_none() {
            self.audio_epoch_media_start_ms = Some(audio_chunk_position_ms(&chunk));
        }

        self.pending_audio = Some(PendingAudioChunk {
            sample_rate: chunk.sample_rate,
            channels: chunk.channels,
            samples: chunk.samples_interleaved,
            offset: 0,
        });

        let _ = self.flush_pending_audio()?;
        Ok(())
    }

    fn flush_pending_audio(&mut self) -> Result<bool, String> {
        if self.pending_audio.is_none() {
            return Ok(true);
        }

        let Some(audio) = self.audio_output.as_mut() else {
            return Err("pending PCM exists without an audio output".to_string());
        };

        if !audio.epoch_ready(self.audio_epoch) {
            return Ok(false);
        }

        let telemetry = audio.telemetry();
        if telemetry.stream_errors > self.audio_last_stream_errors {
            self.audio_last_stream_errors = telemetry.stream_errors;
            return Err("audio output stream reported a device/runtime error".to_string());
        }

        if !self.audio_epoch_armed {
            self.audio_epoch_consumed_base = telemetry.consumed_samples;
            self.audio_epoch_armed = true;
        }

        let done = {
            let pending = self
                .pending_audio
                .as_mut()
                .ok_or_else(|| "pending PCM disappeared".to_string())?;
            if pending.sample_rate != audio.sample_rate()
                || u16::from(pending.channels) != audio.channels()
            {
                return Err("pending PCM format disagrees with active audio device".to_string());
            }
            let remaining = &pending.samples[pending.offset..];
            let written = audio
                .push_pcm(self.audio_epoch, remaining)
                .map_err(|error| format!("audio ring push failed: {error:#}"))?;
            pending.offset = pending.offset.saturating_add(written);
            pending.offset >= pending.samples.len()
        };

        if done {
            self.pending_audio = None;
        }
        Ok(done)
    }

    fn audio_media_position_ms(&self) -> Option<u64> {
        if !self.audio_epoch_armed {
            return None;
        }
        let start_ms = self.audio_epoch_media_start_ms?;
        let audio = self.audio_output.as_ref()?;
        let telemetry = audio.telemetry();
        let played_samples = telemetry
            .consumed_samples
            .saturating_sub(self.audio_epoch_consumed_base);
        let samples_per_second =
            u64::from(audio.sample_rate()).checked_mul(u64::from(audio.channels()))?;
        if samples_per_second == 0 {
            return None;
        }
        Some(
            start_ms.saturating_add(
                played_samples
                    .saturating_mul(1_000)
                    .saturating_div(samples_per_second),
            ),
        )
    }

    fn publish_frame_at_position(&mut self, frame: DecodedVideoFrame, position_ms: u64) {
        let presentation = PresentationFrame {
            generation: self.generation,
            frame,
            presented_position_ms: position_ms,
        };

        let frame_slot_poisoned = match self.frame_slot.lock() {
            Ok(mut slot) => {
                if slot.replace(presentation).is_some() {
                    self.dropped_video_frames = self.dropped_video_frames.saturating_add(1);
                }
                false
            }
            Err(poisoned) => {
                let mut slot = poisoned.into_inner();
                slot.take();
                true
            }
        };
        if frame_slot_poisoned {
            self.fail("playback frame slot mutex was poisoned".to_string());
            return;
        }

        self.presented_position_ms = position_ms;
        self.presented_video_frames = self.presented_video_frames.saturating_add(1);
        let _ = self.event_tx.try_send(PlaybackWorkerEvent::FrameReady {
            generation: self.generation,
        });
        self.emit_snapshot();
    }

    fn push_time_slot_ms(&mut self, slot_ms: u64) -> Result<(), String> {
        push_bounded_time_slot_ms(&mut self.presentation_time_slots_ms, slot_ms)
    }

    fn clear_frame_slot(&self) {
        match self.frame_slot.lock() {
            Ok(mut slot) => {
                slot.take();
            }
            Err(poisoned) => {
                let mut slot = poisoned.into_inner();
                slot.take();
            }
        }
    }

    fn snapshot(&self) -> PlaybackSnapshot {
        let (duration_ms, decoded_position_ms, decoded_video_frames, decoded_audio_chunks) =
            self.session.as_ref().map_or((0, 0, 0, 0), |session| {
                (
                    session.duration_ms(),
                    session.position().as_ms(),
                    session.decoded_video_frames,
                    session.decoded_audio_chunks,
                )
            });

        let audio_telemetry = self.audio_output.as_ref().map(|audio| audio.telemetry());

        PlaybackSnapshot {
            generation: self.generation,
            state: self.state,
            duration_ms,
            presented_position_ms: self.presented_position_ms,
            decoded_position_ms,
            decoded_video_frames,
            decoded_audio_chunks,
            presented_video_frames: self.presented_video_frames,
            dropped_video_frames: self.dropped_video_frames,
            reorder_depth: self.reorder.depth(),
            seek_in_progress: self.state == PlayerState::Seeking,
            audio_media_position_ms: self.audio_media_position_ms(),
            audio_consumed_samples: audio_telemetry.map_or(0, |telemetry| telemetry.consumed_samples),
            audio_underrun_samples: audio_telemetry.map_or(0, |telemetry| telemetry.underrun_samples),
            audio_stream_errors: audio_telemetry.map_or(0, |telemetry| telemetry.stream_errors),
            last_error: None,
        }
    }

    fn emit_snapshot(&self) {
        self.store_snapshot(self.snapshot());
    }

    fn store_snapshot(&self, snapshot: PlaybackSnapshot) {
        match self.snapshot_slot.lock() {
            Ok(mut slot) => *slot = snapshot,
            Err(poisoned) => {
                // The slot contains plain owned state. Recover the inner value and overwrite it
                // so the worker never panics or blocks on a poisoned UI-facing state mutex.
                *poisoned.into_inner() = snapshot;
            }
        }
    }

    fn fail(&mut self, message: String) {
        self.state = PlayerState::Error;
        let mut snapshot = self.snapshot();
        snapshot.last_error = Some(message.clone());
        // Authoritative failure state is persisted before the optional discrete notification.
        self.store_snapshot(snapshot);
        let _ = self.event_tx.try_send(PlaybackWorkerEvent::FatalError {
            generation: self.generation,
            message,
        });
    }
}

fn audio_chunk_position_ms(chunk: &DecodedAudioChunk) -> u64 {
    if chunk.timescale_hz == 0 {
        0
    } else {
        chunk
            .pts_ticks
            .saturating_mul(1_000)
            .saturating_div(u64::from(chunk.timescale_hz))
    }
}

fn frame_position_ms(frame: &DecodedVideoFrame) -> u64 {
    if frame.timescale_hz == 0 {
        0
    } else {
        frame
            .pts_ticks
            .saturating_mul(1000)
            .saturating_div(u64::from(frame.timescale_hz))
    }
}

fn push_bounded_time_slot_ms(slots: &mut VecDeque<u64>, slot_ms: u64) -> Result<(), String> {
    if slots.len() >= MAX_PRESENTATION_REORDER_FRAMES + 1 {
        return Err("presentation timestamp-slot queue exceeded reorder bound".to_string());
    }
    if let Some(previous) = slots.back().copied() {
        if slot_ms < previous {
            return Err(format!(
                "presentation timestamp regressed from {previous} ms to {slot_ms} ms"
            ));
        }
    }
    slots.push_back(slot_ms);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker_with_snapshot_slot(
        snapshot_slot: Arc<Mutex<PlaybackSnapshot>>,
    ) -> PlaybackWorker {
        let (_command_tx, command_rx) = mpsc::sync_channel(1);
        let (event_tx, _event_rx) = mpsc::sync_channel(1);
        PlaybackWorker::new(
            command_rx,
            event_tx,
            Arc::new(Mutex::new(None)),
            snapshot_slot,
            Arc::new(AtomicBool::new(false)),
        )
    }

    struct FakeAudioState {
        requested_epoch: AtomicU64,
        callback_epoch: AtomicU64,
        consumed_samples: AtomicU64,
        underrun_samples: AtomicU64,
        stream_errors: AtomicU64,
        max_write: std::sync::atomic::AtomicUsize,
        paused: AtomicBool,
    }

    impl FakeAudioState {
        fn new(epoch: u64, max_write: usize) -> Arc<Self> {
            Arc::new(Self {
                requested_epoch: AtomicU64::new(epoch),
                callback_epoch: AtomicU64::new(epoch),
                consumed_samples: AtomicU64::new(0),
                underrun_samples: AtomicU64::new(0),
                stream_errors: AtomicU64::new(0),
                max_write: std::sync::atomic::AtomicUsize::new(max_write),
                paused: AtomicBool::new(false),
            })
        }

        fn acknowledge_requested_epoch(&self) {
            let requested = self.requested_epoch.load(Ordering::Acquire);
            self.callback_epoch.store(requested, Ordering::Release);
        }
    }

    struct FakeAudioSink {
        state: Arc<FakeAudioState>,
        sample_rate: u32,
        channels: u16,
    }

    impl FakeAudioSink {
        fn new(state: Arc<FakeAudioState>, sample_rate: u32, channels: u16) -> Self {
            Self {
                state,
                sample_rate,
                channels,
            }
        }
    }

    impl AudioSink for FakeAudioSink {
        fn matches_format(&self, sample_rate: u32, channels: u8) -> bool {
            self.sample_rate == sample_rate && self.channels == u16::from(channels)
        }

        fn request_epoch(&self, epoch: u64) {
            self.state.requested_epoch.store(epoch, Ordering::Release);
        }

        fn epoch_ready(&self, epoch: u64) -> bool {
            self.state.requested_epoch.load(Ordering::Acquire) == epoch
                && self.state.callback_epoch.load(Ordering::Acquire) == epoch
        }

        fn push_pcm(&mut self, epoch: u64, samples: &[i16]) -> anyhow::Result<usize> {
            if !self.epoch_ready(epoch) {
                return Ok(0);
            }
            Ok(samples.len().min(self.state.max_write.load(Ordering::Relaxed)))
        }

        fn telemetry(&self) -> AudioTelemetry {
            AudioTelemetry {
                consumed_samples: self.state.consumed_samples.load(Ordering::Relaxed),
                underrun_samples: self.state.underrun_samples.load(Ordering::Relaxed),
                stream_errors: self.state.stream_errors.load(Ordering::Relaxed),
                requested_epoch: self.state.requested_epoch.load(Ordering::Acquire),
                callback_epoch: self.state.callback_epoch.load(Ordering::Acquire),
            }
        }

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn pause(&self) -> anyhow::Result<()> {
            self.state.paused.store(true, Ordering::Release);
            Ok(())
        }

        fn play(&self) -> anyhow::Result<()> {
            self.state.paused.store(false, Ordering::Release);
            Ok(())
        }
    }

    fn install_fake_audio(
        worker: &mut PlaybackWorker,
        epoch: u64,
        max_write: usize,
    ) -> Arc<FakeAudioState> {
        let state = FakeAudioState::new(epoch, max_write);
        worker.audio_epoch = epoch;
        worker.audio_output = Some(Box::new(FakeAudioSink::new(
            Arc::clone(&state),
            48_000,
            2,
        )));
        state
    }

    fn pending_audio(samples: &[i16]) -> PendingAudioChunk {
        PendingAudioChunk {
            sample_rate: 48_000,
            channels: 2,
            samples: samples.to_vec(),
            offset: 0,
        }
    }

    fn frame(index: u32) -> DecodedVideoFrame {
        DecodedVideoFrame {
            width: 2,
            height: 2,
            frame_index: index,
            pts_ticks: u64::from(index) * 3_000,
            dts_ticks: u64::from(index) * 3_000,
            timescale_hz: 90_000,
            payload_crc32c: index,
            gray8: vec![index as u8; 4],
        }
    }

    #[test]
    fn snapshot_slot_keeps_latest_authoritative_state() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(Arc::clone(&snapshot_slot));

        worker.generation = 7;
        worker.state = PlayerState::Playing;
        worker.presented_position_ms = 120;
        worker.emit_snapshot();

        worker.state = PlayerState::Paused;
        worker.presented_position_ms = 240;
        worker.emit_snapshot();

        let snapshot = snapshot_slot
            .lock()
            .expect("snapshot slot should not be poisoned")
            .clone();
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.state, PlayerState::Paused);
        assert_eq!(snapshot.presented_position_ms, 240);
    }

    #[test]
    fn poisoned_snapshot_slot_is_a_controlled_read_error() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let poison_target = Arc::clone(&snapshot_slot);
        let _ = thread::spawn(move || {
            let _guard = poison_target.lock().expect("lock before poison");
            panic!("intentional snapshot poison");
        })
        .join();

        let (command_tx, _command_rx) = mpsc::sync_channel(1);
        let (_event_tx, event_rx) = mpsc::sync_channel(1);
        let handle = PlaybackWorkerHandle {
            command_tx,
            event_rx,
            frame_slot: Arc::new(Mutex::new(None)),
            snapshot_slot,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
        };

        let error = handle
            .latest_snapshot()
            .expect_err("poisoned snapshot must not be treated as valid state");
        assert!(error.contains("poisoned"));
    }

    #[test]
    fn worker_overwrites_poisoned_snapshot_without_panicking() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let poison_target = Arc::clone(&snapshot_slot);
        let _ = thread::spawn(move || {
            let _guard = poison_target.lock().expect("lock before poison");
            panic!("intentional snapshot poison");
        })
        .join();

        let mut worker = worker_with_snapshot_slot(Arc::clone(&snapshot_slot));
        worker.generation = 9;
        worker.state = PlayerState::Error;
        worker.store_snapshot(worker.snapshot());

        let snapshot = match snapshot_slot.lock() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        assert_eq!(snapshot.generation, 9);
        assert_eq!(snapshot.state, PlayerState::Error);
    }

    #[test]
    fn audio_epoch_change_clears_pending_pcm_and_disarms_clock() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let state = install_fake_audio(&mut worker, 5, usize::MAX);
        worker.pending_audio = Some(pending_audio(&[1, 2, 3]));
        worker.audio_epoch_armed = true;
        worker.audio_epoch_consumed_base = 123;

        worker.advance_audio_epoch(Some(4_000));

        assert!(worker.pending_audio.is_none());
        assert_eq!(worker.audio_epoch, 6);
        assert_eq!(worker.audio_epoch_media_start_ms, Some(4_000));
        assert!(!worker.audio_epoch_armed);
        assert_eq!(worker.audio_epoch_consumed_base, 0);
        assert_eq!(state.requested_epoch.load(Ordering::Acquire), 6);
        assert_eq!(state.callback_epoch.load(Ordering::Acquire), 5);
    }

    #[test]
    fn unacknowledged_audio_epoch_refuses_pcm_and_clock_remains_unarmed() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let _state = install_fake_audio(&mut worker, 10, usize::MAX);
        worker.pending_audio = Some(pending_audio(&[1, 2, 3, 4]));
        worker.advance_audio_epoch(Some(9_000));
        worker.pending_audio = Some(pending_audio(&[1, 2, 3, 4]));

        assert!(!worker.flush_pending_audio().expect("epoch wait"));
        assert_eq!(worker.pending_audio.as_ref().map(|p| p.offset), Some(0));
        assert!(!worker.audio_epoch_armed);
        assert_eq!(worker.audio_media_position_ms(), None);
    }

    #[test]
    fn acknowledged_epoch_captures_fresh_consumed_sample_baseline() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let state = install_fake_audio(&mut worker, 20, usize::MAX);
        state.consumed_samples.store(96_000, Ordering::Relaxed);
        worker.advance_audio_epoch(Some(10_000));
        worker.pending_audio = Some(pending_audio(&[1, 2, 3, 4]));
        state.acknowledge_requested_epoch();

        assert!(worker.flush_pending_audio().expect("flush after ack"));
        assert!(worker.audio_epoch_armed);
        assert_eq!(worker.audio_epoch_consumed_base, 96_000);
        assert_eq!(worker.audio_media_position_ms(), Some(10_000));

        state.consumed_samples.store(144_000, Ordering::Relaxed);
        assert_eq!(worker.audio_media_position_ms(), Some(10_500));
    }

    #[test]
    fn partial_audio_write_retains_pending_pcm_until_drained() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let state = install_fake_audio(&mut worker, 30, 2);
        worker.pending_audio = Some(pending_audio(&[1, 2, 3, 4, 5]));

        assert!(!worker.flush_pending_audio().expect("partial flush"));
        assert_eq!(worker.pending_audio.as_ref().map(|p| p.offset), Some(2));

        state.max_write.store(3, Ordering::Relaxed);
        assert!(worker.flush_pending_audio().expect("final flush"));
        assert!(worker.pending_audio.is_none());
    }

    #[test]
    fn pre_seek_consumed_samples_do_not_shift_post_seek_audio_clock() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let state = install_fake_audio(&mut worker, 40, usize::MAX);
        state.consumed_samples.store(960_000, Ordering::Relaxed);

        worker.advance_audio_epoch(Some(30_000));
        worker.pending_audio = Some(pending_audio(&[1, 2]));
        state.acknowledge_requested_epoch();
        assert!(worker.flush_pending_audio().expect("arm seek epoch"));
        assert_eq!(worker.audio_media_position_ms(), Some(30_000));

        state.consumed_samples.store(1_056_000, Ordering::Relaxed);
        assert_eq!(worker.audio_media_position_ms(), Some(31_000));
    }

    #[test]
    fn stream_error_only_fails_when_counter_increases() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let state = install_fake_audio(&mut worker, 50, 0);
        state.stream_errors.store(3, Ordering::Relaxed);
        worker.audio_last_stream_errors = 3;
        worker.pending_audio = Some(pending_audio(&[1, 2]));

        assert!(!worker.flush_pending_audio().expect("historical error ignored"));

        state.stream_errors.store(4, Ordering::Relaxed);
        let error = worker
            .flush_pending_audio()
            .expect_err("new stream error must fail");
        assert!(error.contains("device/runtime error"));
    }

    #[test]
    fn paused_seek_clock_stays_unarmed_until_epoch_acknowledgement() {
        let snapshot_slot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let mut worker = worker_with_snapshot_slot(snapshot_slot);
        let state = install_fake_audio(&mut worker, 60, usize::MAX);
        state.paused.store(true, Ordering::Release);

        worker.advance_audio_epoch(Some(12_345));
        worker.pending_audio = Some(pending_audio(&[1, 2, 3, 4]));

        assert!(!worker.flush_pending_audio().expect("paused epoch wait"));
        assert_eq!(worker.audio_media_position_ms(), None);

        state.paused.store(false, Ordering::Release);
        state.acknowledge_requested_epoch();
        assert!(worker.flush_pending_audio().expect("resume and arm"));
        assert_eq!(worker.audio_media_position_ms(), Some(12_345));
    }

    #[test]
    fn reorder_restores_single_b_decode_order() {
        let mut reorder = PresentationReorder::new();

        reorder.push(frame(0)).expect("push 0");
        assert_eq!(reorder.pop_ready().map(|f| f.frame_index), Some(0));

        reorder.push(frame(2)).expect("push 2");
        assert!(reorder.pop_ready().is_none());

        reorder.push(frame(1)).expect("push 1");
        assert_eq!(reorder.pop_ready().map(|f| f.frame_index), Some(1));
        assert_eq!(reorder.pop_ready().map(|f| f.frame_index), Some(2));
        assert!(reorder.pop_ready().is_none());
    }

    #[test]
    fn decode_order_time_slots_map_to_display_order() {
        let mut reorder = PresentationReorder::new();
        let mut slots = VecDeque::new();

        let mut i0 = frame(0);
        i0.pts_ticks = 0;
        let mut p2 = frame(2);
        p2.pts_ticks = 3_000;
        let mut b1 = frame(1);
        b1.pts_ticks = 6_000;

        reorder.push(i0).expect("I0");
        slots.push_back(0_u64);
        let first = reorder.pop_ready().expect("display 0");
        assert_eq!((first.frame_index, slots.pop_front()), (0, Some(0)));

        reorder.push(p2).expect("P2");
        slots.push_back(33);
        assert!(reorder.pop_ready().is_none());

        reorder.push(b1).expect("B1");
        slots.push_back(66);
        let second = reorder.pop_ready().expect("display 1");
        let third = reorder.pop_ready().expect("display 2");
        assert_eq!((second.frame_index, slots.pop_front()), (1, Some(33)));
        assert_eq!((third.frame_index, slots.pop_front()), (2, Some(66)));
        assert!(slots.is_empty());
    }

    #[test]
    fn reorder_marks_already_presented_frame_as_discarded() {
        let mut reorder = PresentationReorder::new();
        assert!(reorder.push(frame(0)).expect("frame 0 accepted"));
        assert_eq!(reorder.pop_ready().map(|f| f.frame_index), Some(0));
        assert!(!reorder.push(frame(0)).expect("old frame is discarded"));
    }

    #[test]
    fn reorder_rejects_duplicate_index() {
        let mut reorder = PresentationReorder::new();
        reorder.reset(Some(1));
        reorder.push(frame(2)).expect("first frame");
        let error = reorder.push(frame(2)).expect_err("duplicate must fail");
        assert!(error.contains("duplicate"));
    }

    #[test]
    fn reorder_rejects_gap_beyond_bound() {
        let mut reorder = PresentationReorder::new();
        reorder.reset(Some(0));
        let error = reorder
            .push(frame((MAX_PRESENTATION_REORDER_FRAMES + 1) as u32))
            .expect_err("oversized gap must fail");
        assert!(error.contains("exceeds reorder bound"));
    }

    #[test]
    fn eos_rejects_unresolved_gap() {
        let mut reorder = PresentationReorder::new();
        reorder.reset(Some(1));
        reorder.push(frame(2)).expect("buffer future frame");
        assert!(reorder.finish_eos().is_err());
    }

    #[test]
    fn presentation_floor_discards_older_recovery_frames() {
        let mut reorder = PresentationReorder::new();
        reorder.reset(Some(5));
        reorder.push(frame(3)).expect("older recovery frame");
        reorder.push(frame(5)).expect("target frame");
        assert_eq!(reorder.pop_ready().map(|f| f.frame_index), Some(5));
    }
}
