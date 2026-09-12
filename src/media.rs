use std::{
    collections::VecDeque,
    io,
    os::raw::{c_float, c_int, c_uint},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use vivid_protocol::{
    media::{self as protocol_media, AudioPacket},
    messages::LaneClass,
    track::{
        AudioConfiguration, KindConfiguration, RasterConfiguration, TrackConfiguration, TrackMode,
    },
};
use vivid_sdk::{
    CoordinateModel, MILESTONE_OUTPUT_READY, RequestMetadata, SceneNode, Session, SessionEvent,
    SlotBinding, Surface, SurfaceDefinition, SurfaceDescriptor, SurfaceRole, Track, TrackChannel,
    TrackWaitCondition,
};

use crate::{
    cli::Args,
    client,
    geometry::{self, DOOM_HEIGHT, DOOM_WIDTH, FrameLayout, TerminalGeometry},
};

const VIDEO_RATE: u64 = 35;
/// The declared raster period, enforced before a frame is taken rather than after.
///
/// `send_raster_adaptive` blocks inside the channel's declared-rate limiter, so a frame taken
/// before that block is already a frame old by the time the record goes out. Doom offers frames
/// faster than the declared rate — its renderer shimmers between the two draws of one 35 Hz tic —
/// which keeps the limiter permanently saturated and makes that staleness permanent. Waiting the
/// period out first and taking the frame afterwards costs the same bandwidth and sends the newest
/// image instead of the one that was newest a frame ago.
const VIDEO_FRAME_PERIOD: Duration = Duration::from_nanos(1_000_000_000 / VIDEO_RATE);
const VIDEO_SLOT: u64 = 3;
const AUDIO_SLOT: u64 = 2;
const MEDIA_EPOCH: u32 = 1;
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u8 = 2;
const AUDIO_FRAME_US: u64 = 20_000;
const AUDIO_FRAMES_PER_PACKET: usize = 960;
const AUDIO_SAMPLES_PER_PACKET: usize = AUDIO_FRAMES_PER_PACKET * AUDIO_CHANNELS as usize;
const AUDIO_PACKET_BYTES: u32 = (AUDIO_SAMPLES_PER_PACKET * size_of::<f32>()) as u32;
const AUDIO_QUEUE_PACKETS: usize = 8;
const ACTIVATION_TIMEOUT_US: u64 = 30_000_000;
const GRACEFUL_WORKER_TIMEOUT: Duration = Duration::from_millis(250);
const TRACK_FAILURE_DIAGNOSTIC_WAIT: Duration = Duration::from_millis(100);

unsafe extern "C" {
    fn VVDOOM_AudioReady() -> c_int;
    fn VVDOOM_ReadAudioFrames(output: *mut c_float, frame_count: c_uint) -> c_uint;
    #[cfg(test)]
    fn VVDOOM_NonzeroAudioSamples() -> u64;
}

pub struct Presentation {
    session: Session,
    surface: Surface,
    node: SceneNode,
    video_track: Track,
    video_channel: TrackChannel,
    audio_track: Option<Track>,
    audio_channel: Option<TrackChannel>,
    geometry: TerminalGeometry,
    scale: bool,
    activated: bool,
    video_queue: Arc<LatestQueue<Vec<u8>>>,
    audio_queue: Arc<BoundedQueue<AudioBlock>>,
    status: Arc<WorkerStatus>,
    workers: Vec<JoinHandle<()>>,
    worker_done_tx: mpsc::Sender<()>,
    worker_done_rx: mpsc::Receiver<()>,
    verbose: bool,
}

impl Presentation {
    pub fn connect(args: &Args, sound_enabled: bool) -> io::Result<Self> {
        Self::from_session(client::connect(args)?, sound_enabled, args.verbose)
    }

    pub(crate) fn from_session(
        mut session: Session,
        sound_enabled: bool,
        verbose: bool,
    ) -> io::Result<Self> {
        let context_id = session.info().root_context_id;
        let surface_id = session.allocate_id()?;
        let node_id = session.allocate_id()?;
        let surface = session.create_surface(
            SurfaceDefinition {
                context_id,
                surface_id,
                semantic_profile: vivid_sdk::GENERIC_CONTENT.into(),
                coordinate_model: CoordinateModel::DesktopLogicalPixels,
                logical_width: u64::from(DOOM_WIDTH),
                logical_height: u64::from(DOOM_HEIGHT),
                scale_numerator: 1,
                scale_denominator: 1,
                rotation: 0,
                descriptor: SurfaceDescriptor {
                    role: SurfaceRole::ApplicationCanvas,
                    title: "Doom".into(),
                    semantic_content_revision: 1,
                    semantic_availability: 0,
                    locator_hint: String::new(),
                },
                policy: 0,
                profile_parameters: vec![],
            },
            &RequestMetadata::default(),
        )?;
        let (geometry, mut node) = create_terminal_node(&mut session, &surface, node_id, true)?;

        let video_configuration = raster_configuration(&session, &surface)?;
        if !session
            .probe_track(&probe_configuration(&video_configuration))?
            .supported
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "presenter does not support the required live RGBA raster track",
            ));
        }
        let video_track = session.create_track(video_configuration, &RequestMetadata::default())?;
        let video_channel = session.open_track_channel(&video_track)?;

        let (audio_track, audio_channel) = if sound_enabled {
            let configuration = audio_configuration(&session, &surface)?;
            if !session
                .probe_track(&probe_configuration(&configuration))?
                .supported
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "presenter does not support live pcm_f32le audio; rerun vvdoom with -nosound",
                ));
            }
            let track = session.create_track(configuration, &RequestMetadata::default())?;
            let channel = session.open_track_channel(&track)?;
            (Some(track), Some(channel))
        } else {
            (None, None)
        };

        let (worker_done_tx, worker_done_rx) = mpsc::channel();
        // Keep node mutable after construction; target updates rewrite its grid rectangle.
        node.fit = vivid_sdk::Fit::Contain;
        Ok(Self {
            session,
            surface,
            node,
            video_track,
            video_channel,
            audio_track,
            audio_channel,
            geometry,
            scale: true,
            activated: false,
            video_queue: Arc::new(LatestQueue::new()),
            audio_queue: Arc::new(BoundedQueue::new(AUDIO_QUEUE_PACKETS)),
            status: Arc::new(WorkerStatus::default()),
            workers: Vec::new(),
            worker_done_tx,
            worker_done_rx,
            verbose,
        })
    }

    pub fn start_workers(&mut self) -> io::Result<()> {
        if !self.workers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "media workers already started",
            ));
        }
        self.spawn_video_worker()?;
        if let Some(channel) = self.audio_channel.clone() {
            self.spawn_audio_mixer()?;
            self.spawn_audio_sender(channel)?;
        }
        Ok(())
    }

    pub fn activate(&mut self) -> io::Result<()> {
        self.wait_output_ready(&self.video_track.clone())?;
        let mut bindings = vec![SlotBinding {
            slot: VIDEO_SLOT,
            track_id: self.video_track.id(),
            expected_channel_generation: self.video_channel.generation(),
            required_milestone: MILESTONE_OUTPUT_READY,
        }];
        if let Some(track) = self.audio_track.clone() {
            match self.wait_output_ready(&track) {
                Ok(()) => {
                    let channel = self.audio_channel.as_ref().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "Vivid audio channel disappeared before activation",
                        )
                    })?;
                    bindings.push(SlotBinding {
                        slot: AUDIO_SLOT,
                        track_id: track.id(),
                        expected_channel_generation: channel.generation(),
                        required_milestone: MILESTONE_OUTPUT_READY,
                    });
                }
                Err(error) => self
                    .disable_audio_track(&format!("audio did not become ready: {error}"), false)?,
            }
        }
        self.session
            .activate_tracks(&self.surface, &bindings, &RequestMetadata::default())?;
        self.activated = true;
        self.log(format_args!(
            "activated 640x400 raster{}",
            if self.audio_track.is_some() {
                " with 48 kHz stereo Vivid audio"
            } else {
                " without audio"
            }
        ));
        Ok(())
    }

    pub fn submit_frame(&self, rgba: &[u8]) -> io::Result<()> {
        let expected = frame_pixel_bytes()?;
        if rgba.len() != expected as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Doom frame does not contain exactly 640x400 RGBA pixels",
            ));
        }
        self.video_queue.push(rgba.to_vec())
    }

    pub fn layout(&self) -> FrameLayout {
        self.geometry.layout(self.scale)
    }

    pub fn toggle_scale(&mut self) -> io::Result<()> {
        self.scale = !self.scale;
        self.update_layout()
    }

    pub fn poll(&mut self) -> io::Result<()> {
        let fatal_error = self.status.take_fatal_error();
        let audio_error = self.status.take_audio_error();
        let deadline = fatal_error
            .as_ref()
            .or(audio_error.as_ref())
            .map(|_| Instant::now() + TRACK_FAILURE_DIAGNOSTIC_WAIT);
        loop {
            while let Some(event) = self.session.take_event()? {
                self.handle_event(event)?;
            }
            if fatal_error.is_none() && audio_error.is_none() {
                return Ok(());
            }
            if audio_error.is_some() && self.audio_track.is_none() && fatal_error.is_none() {
                return Ok(());
            }
            if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
                if let Some(error) = fatal_error {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, error));
                }
                if let Some(error) = audio_error {
                    self.disable_audio_track(&error, true)?;
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn handle_event(&mut self, event: SessionEvent) -> io::Result<()> {
        match event {
            SessionEvent::TargetChanged(payload) => {
                self.session.apply_target_changed(&payload)?;
                self.geometry =
                    TerminalGeometry::from_descriptor(&self.session.info().target_descriptor)?;
                self.update_layout()
            }
            SessionEvent::TrackLost { object_id, payload }
                if object_id == self.video_track.id() =>
            {
                let diagnostic = payload
                    .iter()
                    .find(|(key, _)| *key == 6)
                    .and_then(|(_, value)| value.as_text())
                    .unwrap_or("presenter supplied no diagnostic");
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("presenter lost Vivid raster track: {diagnostic}"),
                ))
            }
            SessionEvent::TrackLost { object_id, payload }
                if self
                    .audio_track
                    .as_ref()
                    .is_some_and(|track| object_id == track.id()) =>
            {
                let diagnostic = payload
                    .iter()
                    .find(|(key, _)| *key == 6)
                    .and_then(|(_, value)| value.as_text())
                    .unwrap_or("presenter supplied no diagnostic");
                self.disable_audio_track(diagnostic, true)
            }
            SessionEvent::ConnectionClosed { diagnostic } => {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, diagnostic))
            }
            _ => Ok(()),
        }
    }

    fn disable_audio_track(&mut self, reason: &str, rebind_video: bool) -> io::Result<()> {
        let Some(track) = self.audio_track.take() else {
            return Ok(());
        };
        self.audio_queue.close();
        if let Some(channel) = self.audio_channel.take() {
            let _ = channel.close();
        }
        if rebind_video && self.activated {
            self.session.activate_tracks(
                &self.surface,
                &[SlotBinding {
                    slot: VIDEO_SLOT,
                    track_id: self.video_track.id(),
                    expected_channel_generation: self.video_channel.generation(),
                    required_milestone: MILESTONE_OUTPUT_READY,
                }],
                &RequestMetadata::default(),
            )?;
        }
        let _ = self
            .session
            .destroy_track(&track, &RequestMetadata::default());
        self.log(format_args!(
            "Vivid audio disabled ({reason}); gameplay continues without sound"
        ));
        Ok(())
    }

    fn update_layout(&mut self) -> io::Result<()> {
        let layout = self.layout();
        match geometry::update_scene_node(&mut self.session, &mut self.node, layout) {
            Ok(()) => Ok(()),
            Err(error)
                if presenter_code(&error)
                    == Some(vivid_protocol::registry::error::STALE_TARGET_GENERATION) =>
            {
                let mut applied = false;
                while let Some(event) = self.session.take_event()? {
                    match event {
                        SessionEvent::TargetChanged(payload) => {
                            self.session.apply_target_changed(&payload)?;
                            applied = true;
                        }
                        SessionEvent::ConnectionClosed { diagnostic } => {
                            return Err(io::Error::new(io::ErrorKind::BrokenPipe, diagnostic));
                        }
                        SessionEvent::TrackLost { object_id, payload }
                            if self
                                .audio_track
                                .as_ref()
                                .is_some_and(|track| object_id == track.id()) =>
                        {
                            let diagnostic = payload
                                .iter()
                                .find(|(key, _)| *key == 6)
                                .and_then(|(_, value)| value.as_text())
                                .unwrap_or("presenter supplied no diagnostic");
                            self.disable_audio_track(diagnostic, true)?;
                        }
                        SessionEvent::TrackLost { object_id, .. } => {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                format!("presenter lost Vivid track {object_id}"),
                            ));
                        }
                        _ => {}
                    }
                }
                if !applied {
                    return Err(error);
                }
                self.geometry =
                    TerminalGeometry::from_descriptor(&self.session.info().target_descriptor)?;
                let layout = self.layout();
                geometry::update_scene_node(&mut self.session, &mut self.node, layout)
            }
            Err(error) => Err(error),
        }
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.status.stop.store(true, Ordering::Release);
        self.video_queue.close();
        self.audio_queue.close();
        let deadline = Instant::now() + GRACEFUL_WORKER_TIMEOUT;
        let mut completed = 0;
        while completed < self.workers.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || self.worker_done_rx.recv_timeout(remaining).is_err() {
                break;
            }
            completed += 1;
        }
        if completed != self.workers.len() {
            self.session.abort()?;
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        self.session.close()
    }

    fn wait_output_ready(&mut self, track: &Track) -> io::Result<()> {
        let satisfied = self.session.wait_track(
            track,
            TrackWaitCondition::MilestoneSet,
            Some(MILESTONE_OUTPUT_READY),
            ACTIVATION_TIMEOUT_US,
        )?;
        if satisfied.observed_value.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for Vivid output readiness",
            ));
        }
        Ok(())
    }

    fn spawn_video_worker(&mut self) -> io::Result<()> {
        let queue = self.video_queue.clone();
        let channel = self.video_channel.clone();
        let status = self.status.clone();
        let done = self.worker_done_tx.clone();
        let join = thread::Builder::new()
            .name("vvdoom-vivid-raster".into())
            .spawn(move || {
                let mut frame_id = 0_u64;
                let mut next_send = Instant::now();
                loop {
                    if let Some(wait) = next_send.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    let Some(frame) = queue.pop() else { break };
                    next_send = next_raster_deadline(next_send, Instant::now());
                    frame_id = match frame_id.checked_add(1) {
                        Some(value) => value,
                        None => {
                            status.fail_fatal("Vivid raster frame identity space exhausted");
                            break;
                        }
                    };
                    if let Err(error) = channel.send_raster_adaptive(MEDIA_EPOCH, frame_id, &frame)
                    {
                        if !status.stop.load(Ordering::Acquire) {
                            status.fail_fatal(format!("raster sender failed: {error}"));
                        }
                        break;
                    }
                }
                let _ = channel.eos();
                let _ = done.send(());
            })?;
        self.workers.push(join);
        Ok(())
    }

    fn spawn_audio_mixer(&mut self) -> io::Result<()> {
        let queue = self.audio_queue.clone();
        let status = self.status.clone();
        let done = self.worker_done_tx.clone();
        let join = thread::Builder::new()
            .name("vvdoom-audio-mixer".into())
            .spawn(move || {
                let origin = Instant::now();
                let mut packet_index = 0_u64;
                while !status.stop.load(Ordering::Acquire) {
                    // Doom initializes the headless engine during doomgeneric_Create.
                    if unsafe { VVDOOM_AudioReady() } == 0 {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    let mut samples = vec![0.0_f32; AUDIO_SAMPLES_PER_PACKET];
                    let frames = unsafe {
                        VVDOOM_ReadAudioFrames(
                            samples.as_mut_ptr(),
                            AUDIO_FRAMES_PER_PACKET as c_uint,
                        )
                    };
                    if frames != AUDIO_FRAMES_PER_PACKET as c_uint {
                        status.fail_audio("headless miniaudio mixer returned a short PCM block");
                        break;
                    }
                    let data = pcm_f32le(&samples);
                    let pts_us = packet_index
                        .checked_mul(AUDIO_FRAME_US)
                        .and_then(|value| i64::try_from(value).ok());
                    let Some(pts_us) = pts_us else {
                        status.fail_audio("Vivid audio timeline exhausted");
                        break;
                    };
                    if queue.push(AudioBlock { pts_us, data }).is_err() {
                        break;
                    }
                    packet_index = match packet_index.checked_add(1) {
                        Some(value) => value,
                        None => {
                            status.fail_audio("Vivid audio packet identity space exhausted");
                            break;
                        }
                    };
                    let deadline =
                        origin + Duration::from_micros(packet_index.saturating_mul(AUDIO_FRAME_US));
                    if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
                        thread::sleep(wait.min(Duration::from_millis(20)));
                    }
                }
                queue.close();
                let _ = done.send(());
            })?;
        self.workers.push(join);
        Ok(())
    }

    fn spawn_audio_sender(&mut self, channel: TrackChannel) -> io::Result<()> {
        let queue = self.audio_queue.clone();
        let status = self.status.clone();
        let done = self.worker_done_tx.clone();
        let join = thread::Builder::new()
            .name("vvdoom-vivid-audio".into())
            .spawn(move || {
                let mut packet_id = 0_u64;
                while let Some(block) = queue.pop() {
                    packet_id = match packet_id.checked_add(1) {
                        Some(value) => value,
                        None => {
                            status.fail_audio("Vivid audio packet identity space exhausted");
                            break;
                        }
                    };
                    let result = channel.send_audio(AudioPacket {
                        epoch: MEDIA_EPOCH,
                        packet_id,
                        pts_us: block.pts_us,
                        dts_us: block.pts_us,
                        duration_us: AUDIO_FRAME_US,
                        trim_start_samples: 0,
                        trim_end_samples: 0,
                        data: &block.data,
                    });
                    if let Err(error) = result {
                        if !status.stop.load(Ordering::Acquire) {
                            status.fail_audio(format!("audio sender failed: {error}"));
                        }
                        break;
                    }
                }
                let _ = channel.eos();
                let _ = done.send(());
            })?;
        self.workers.push(join);
        Ok(())
    }

    fn log(&self, message: std::fmt::Arguments<'_>) {
        if self.verbose {
            eprintln!("vvdoom: {message}");
        }
    }
}

#[cfg(test)]
pub(crate) fn nonzero_mixer_samples() -> u64 {
    // SAFETY: the bridge returns one integer counter and does not dereference caller memory.
    unsafe { VVDOOM_NonzeroAudioSamples() }
}

fn create_terminal_node(
    session: &mut Session,
    surface: &Surface,
    node_id: u64,
    scale: bool,
) -> io::Result<(TerminalGeometry, SceneNode)> {
    for _ in 0..2 {
        let terminal = TerminalGeometry::from_descriptor(&session.info().target_descriptor)?;
        let node = geometry::scene_node(session, surface, node_id, terminal.layout(scale))?;
        match session.create_node(&node, &RequestMetadata::default()) {
            Ok(_) => return Ok((terminal, node)),
            Err(error)
                if presenter_code(&error)
                    == Some(vivid_protocol::registry::error::STALE_TARGET_GENERATION) =>
            {
                let mut applied = false;
                while let Some(event) = session.take_event()? {
                    match event {
                        SessionEvent::TargetChanged(payload) => {
                            session.apply_target_changed(&payload)?;
                            applied = true;
                        }
                        SessionEvent::ConnectionClosed { diagnostic } => {
                            return Err(io::Error::new(io::ErrorKind::BrokenPipe, diagnostic));
                        }
                        _ => {}
                    }
                }
                if !applied {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "terminal target kept changing while vvdoom created its scene node",
    ))
}

fn raster_configuration(session: &Session, surface: &Surface) -> io::Result<TrackConfiguration> {
    let maximum_record_body = protocol_media::rgba8_raw_frame_body_len(DOOM_WIDTH, DOOM_HEIGHT)
        .map_err(io::Error::other)?;
    let retained_pixel_charge = u64::from(DOOM_WIDTH)
        .checked_mul(u64::from(DOOM_HEIGHT))
        .ok_or_else(|| invalid_input("raster pixel claim overflows"))?;
    let bits_per_second = u64::from(maximum_record_body)
        .checked_mul(8)
        .and_then(|bits| bits.checked_mul(VIDEO_RATE))
        .ok_or_else(|| invalid_input("raster bitrate claim overflows"))?;
    Ok(TrackConfiguration {
        direction: Default::default(),
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: session.allocate_id()?,
        slot: VIDEO_SLOT,
        mode: TrackMode::Live,
        lane: LaneClass::Bulk,
        maximum_record_body,
        maximum_rate_millihertz: VIDEO_RATE * 1_000,
        maximum_encoded_bits_per_second: bits_per_second,
        maximum_records_per_second: VIDEO_RATE,
        maximum_inflight_body_bytes: u64::from(maximum_record_body).saturating_mul(2),
        kind: KindConfiguration::Raster(RasterConfiguration {
            width: DOOM_WIDTH,
            height: DOOM_HEIGHT,
            alpha_mode: 1,
            delta_enabled: false,
            maximum_delta_operations: 1,
            zstd_enabled: true,
        }),
        target_latency_us: 0,
        maximum_latency_us: 100_000,
        retained_pixel_charge,
    })
}

fn audio_configuration(session: &Session, surface: &Surface) -> io::Result<TrackConfiguration> {
    let maximum_record_body =
        protocol_media::audio_body_len(AUDIO_PACKET_BYTES).map_err(io::Error::other)?;
    // Admission accounts for the complete Vivid record, not only its PCM access unit. Claim the
    // fixed audio header as well or a sustained 50 Hz stream eventually exceeds its own bucket.
    let bits_per_second = u64::from(maximum_record_body)
        .checked_mul(8)
        .and_then(|bits| bits.checked_mul(1_000_000 / AUDIO_FRAME_US))
        .ok_or_else(|| invalid_input("audio bitrate claim overflows"))?;
    Ok(TrackConfiguration {
        direction: Default::default(),
        context_id: surface.context_id(),
        surface_id: surface.id(),
        track_id: session.allocate_id()?,
        slot: AUDIO_SLOT,
        mode: TrackMode::Live,
        lane: LaneClass::Realtime,
        maximum_record_body,
        maximum_rate_millihertz: (1_000_000 / AUDIO_FRAME_US) * 1_000,
        maximum_encoded_bits_per_second: bits_per_second,
        maximum_records_per_second: 1_000_000 / AUDIO_FRAME_US,
        maximum_inflight_body_bytes: u64::from(maximum_record_body)
            .saturating_mul(AUDIO_QUEUE_PACKETS as u64),
        kind: KindConfiguration::Audio(AudioConfiguration {
            codec: "pcm_f32le".into(),
            packetization: "pcm-packet-v1".into(),
            extradata: Vec::new(),
            sample_rate: AUDIO_RATE,
            channels: AUDIO_CHANNELS,
            channel_mask: 3,
            maximum_access_unit_bytes: AUDIO_PACKET_BYTES,
            codec_string: Some("pcm-f32".into()),
        }),
        target_latency_us: 40_000,
        maximum_latency_us: 250_000,
        retained_pixel_charge: 0,
    })
}

/// When the raster worker may take its next frame, given when it took this one.
///
/// Advancing by exactly one period keeps the declared rate met rather than undershot. Clamping to
/// `taken` stops an idle stretch from banking credit for a burst, so the first frame after a still
/// screen goes out the moment Doom draws it instead of waiting out a schedule that ran on without
/// it.
fn next_raster_deadline(previous: Instant, taken: Instant) -> Instant {
    (previous + VIDEO_FRAME_PERIOD).max(taken)
}

fn frame_pixel_bytes() -> io::Result<u32> {
    DOOM_WIDTH
        .checked_mul(DOOM_HEIGHT)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| invalid_input("Doom RGBA frame size overflows"))
}

fn probe_configuration(configuration: &TrackConfiguration) -> TrackConfiguration {
    let mut probe = configuration.clone();
    probe.track_id = 0;
    probe
}

fn pcm_f32le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len().saturating_mul(size_of::<f32>()));
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn presenter_code(error: &io::Error) -> Option<u64> {
    error
        .get_ref()
        .and_then(|source| source.downcast_ref::<vivid_sdk::PresenterError>())
        .map(|error| error.code)
}

#[derive(Debug)]
struct AudioBlock {
    pts_us: i64,
    data: Vec<u8>,
}

#[derive(Default)]
struct WorkerStatus {
    stop: AtomicBool,
    fatal_error: Mutex<Option<String>>,
    audio_error: Mutex<Option<String>>,
}

impl WorkerStatus {
    fn fail_fatal(&self, message: impl Into<String>) {
        if let Ok(mut error) = self.fatal_error.lock()
            && error.is_none()
        {
            *error = Some(message.into());
        }
    }

    fn fail_audio(&self, message: impl Into<String>) {
        if let Ok(mut error) = self.audio_error.lock()
            && error.is_none()
        {
            *error = Some(message.into());
        }
    }

    fn take_fatal_error(&self) -> Option<String> {
        self.fatal_error.lock().ok()?.take()
    }

    fn take_audio_error(&self) -> Option<String> {
        self.audio_error.lock().ok()?.take()
    }
}

struct LatestQueue<T> {
    state: Mutex<LatestState<T>>,
    ready: Condvar,
}

struct LatestState<T> {
    item: Option<T>,
    closed: bool,
}

impl<T> LatestQueue<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(LatestState {
                item: None,
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn push(&self, item: T) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("latest-frame queue is poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "latest-frame queue is closed",
            ));
        }
        state.item = Some(item);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<T> {
        let mut state = self.state.lock().ok()?;
        while state.item.is_none() && !state.closed {
            state = self.ready.wait(state).ok()?;
        }
        state.item.take()
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.ready.notify_all();
        }
    }
}

struct BoundedQueue<T> {
    capacity: usize,
    state: Mutex<BoundedState<T>>,
    ready: Condvar,
}

struct BoundedState<T> {
    items: VecDeque<T>,
    closed: bool,
}

impl<T> BoundedQueue<T> {
    fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            state: Mutex::new(BoundedState {
                items: VecDeque::with_capacity(capacity),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn push(&self, item: T) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("bounded media queue is poisoned"))?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "bounded media queue is closed",
            ));
        }
        if state.items.len() == self.capacity {
            state.items.pop_front();
        }
        state.items.push_back(item);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Option<T> {
        let mut state = self.state.lock().ok()?;
        while state.items.is_empty() && !state.closed {
            state = self.ready.wait(state).ok()?;
        }
        state.items.pop_front()
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.ready.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    use vivid_sdk::testing::{ROOT_SECRET_HEX, TestPresenter};
    use vivid_sdk::{ProducerAuthentication, ProducerConfig};

    #[test]
    fn latest_queue_replaces_stale_frame() {
        let queue = LatestQueue::new();
        queue.push(1).unwrap();
        queue.push(2).unwrap();
        assert_eq!(queue.pop(), Some(2));
    }

    #[test]
    fn bounded_queue_drops_oldest_audio() {
        let queue = BoundedQueue::new(2);
        queue.push(1).unwrap();
        queue.push(2).unwrap();
        queue.push(3).unwrap();
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), Some(3));
    }

    #[test]
    fn raster_pacing_holds_the_declared_rate_without_banking_idle_credit() {
        let start = Instant::now();
        // Frames offered faster than the declared rate are taken exactly one period apart, so the
        // channel's own limiter always has capacity and never holds a frame past its freshness.
        let mut deadline = start;
        for step in 1..=4 {
            deadline = next_raster_deadline(deadline, start);
            assert_eq!(deadline, start + VIDEO_FRAME_PERIOD * step);
        }

        // A still screen leaves the schedule far behind. The frame that ends it is not made to
        // wait out the periods that elapsed with nothing to send.
        let resumed = start + Duration::from_secs(5);
        assert_eq!(next_raster_deadline(deadline, resumed), resumed);
    }

    #[test]
    fn checked_claims_cover_raw_raster_and_pcm() {
        assert_eq!(frame_pixel_bytes().unwrap(), 1_024_000);
        assert_eq!(AUDIO_PACKET_BYTES, 7_680);
        assert_eq!(VIDEO_RATE * 1_000, 35_000);
        assert_eq!(
            VIDEO_FRAME_PERIOD * VIDEO_RATE as u32,
            Duration::from_nanos(999_999_980)
        );
        assert_eq!(1_000_000 / AUDIO_FRAME_US, 50);
    }

    #[test]
    fn pcm_f32_is_little_endian_and_lossless() {
        let samples = [0.25_f32, -0.75_f32, 1.0_f32];
        let encoded = pcm_f32le(&samples);
        let decoded = encoded
            .as_chunks::<{ size_of::<f32>() }>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect::<Vec<_>>();
        assert_eq!(decoded, samples);
    }

    #[test]
    fn test_presenter_accepts_raster_activation_resize_and_eos() {
        let presenter = TestPresenter::start(80, 24).unwrap();
        let endpoint = presenter.endpoint().to_owned();
        let session = Session::connect(ProducerConfig {
            endpoint_control: Some(endpoint.clone()),
            endpoint_realtime: Some(endpoint.clone()),
            endpoint_bulk: Some(endpoint),
            authentication: ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            producer_name: "vvdoom-test".into(),
            producer_version: env!("CARGO_PKG_VERSION").into(),
            ..ProducerConfig::default()
        })
        .unwrap();
        let mut presentation = Presentation::from_session(session, false, false).unwrap();
        presentation.start_workers().unwrap();
        presentation
            .submit_frame(&vec![0; frame_pixel_bytes().unwrap() as usize])
            .unwrap();
        presentation.activate().unwrap();

        presenter.resize_terminal(100, 40, true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while presentation.geometry.cols != 100 && Instant::now() < deadline {
            presentation.poll().unwrap();
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            (presentation.geometry.cols, presentation.geometry.rows),
            (100, 40)
        );
        presentation.shutdown().unwrap();

        let channels = presenter.channels();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].media_records, 1);
    }

    #[test]
    fn test_presenter_accepts_headless_pcm_track_configuration() {
        let presenter = TestPresenter::start(80, 24).unwrap();
        let endpoint = presenter.endpoint().to_owned();
        let session = Session::connect(ProducerConfig {
            endpoint_control: Some(endpoint.clone()),
            endpoint_realtime: Some(endpoint.clone()),
            endpoint_bulk: Some(endpoint),
            authentication: ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            producer_name: "vvdoom-audio-test".into(),
            producer_version: env!("CARGO_PKG_VERSION").into(),
            ..ProducerConfig::default()
        })
        .unwrap();
        let presentation = Presentation::from_session(session, true, false).unwrap();
        let track_configuration = presentation
            .audio_track
            .as_ref()
            .unwrap()
            .configuration()
            .unwrap();
        assert_eq!(
            track_configuration.maximum_encoded_bits_per_second,
            u64::from(track_configuration.maximum_record_body) * 8 * 50
        );
        let KindConfiguration::Audio(audio) = track_configuration.kind else {
            panic!("vvdoom audio track is not audio");
        };
        assert_eq!(audio.codec, "pcm_f32le");
        assert_eq!(audio.codec_string.as_deref(), Some("pcm-f32"));
        assert_eq!((audio.sample_rate, audio.channels), (48_000, 2));
        presentation.shutdown().unwrap();
    }

    #[test]
    fn closed_audio_channel_degrades_to_silent_gameplay() {
        let presenter = TestPresenter::start(80, 24).unwrap();
        let endpoint = presenter.endpoint().to_owned();
        let session = Session::connect(ProducerConfig {
            endpoint_control: Some(endpoint.clone()),
            endpoint_realtime: Some(endpoint.clone()),
            endpoint_bulk: Some(endpoint),
            authentication: ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            producer_name: "vvdoom-audio-loss-test".into(),
            producer_version: env!("CARGO_PKG_VERSION").into(),
            ..ProducerConfig::default()
        })
        .unwrap();
        let mut presentation = Presentation::from_session(session, true, false).unwrap();
        presentation
            .audio_channel
            .as_ref()
            .unwrap()
            .close()
            .unwrap();
        presentation
            .spawn_audio_sender(presentation.audio_channel.clone().unwrap())
            .unwrap();
        presentation
            .audio_queue
            .push(AudioBlock {
                pts_us: 0,
                data: vec![0; AUDIO_PACKET_BYTES as usize],
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while presentation.audio_track.is_some() && Instant::now() < deadline {
            presentation.poll().unwrap();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(presentation.audio_track.is_none());
        presentation.shutdown().unwrap();
    }
}
