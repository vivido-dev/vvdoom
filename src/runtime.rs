use std::{
    borrow::Cow,
    ffi::{CStr, CString},
    io::{self, Write},
    os::raw::{c_char, c_int, c_uint},
    path::Path,
    ptr,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};

use crate::{
    geometry::{DOOM_HEIGHT, DOOM_WIDTH},
    input,
    media::Presentation,
    terminal,
};

const DOOM_PIXELS: usize = DOOM_WIDTH as usize * DOOM_HEIGHT as usize;
const RGBA_FRAME_BYTES: usize = DOOM_PIXELS * 4;
const KEY_QUEUE_LEN: usize = 32;
/// How long an unchanged framebuffer may go unsent.
///
/// The presenter retains the latest full frame, so a still image needs no repeats to stay on
/// screen. A channel that recovers into a new generation does need one, and Doom can sit on a
/// motionless menu indefinitely, so re-send the retained image at a rate that costs nothing
/// against the declared 35 records/s rather than leaving a recovered channel blank.
const FRAME_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

static STATE: OnceLock<Mutex<RuntimeState>> = OnceLock::new();
static EXIT_FLAG: AtomicBool = AtomicBool::new(false);
static SIGNAL_HANDLERS_INSTALLED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
enum DoomEventType {
    KeyDown = 0,
    KeyUp = 1,
    Mouse = 2,
    Joystick = 3,
    Quit = 4,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DoomEvent {
    event_type: DoomEventType,
    data1: c_int,
    data2: c_int,
    data3: c_int,
    data4: c_int,
}

unsafe extern "C" {
    static mut DG_ScreenBuffer: *mut u32;

    fn doomgeneric_Create(argc: c_int, argv: *mut *mut c_char);
    fn doomgeneric_Tick();
    fn D_PostEvent(ev: *mut DoomEvent);
    fn VVDOOM_SetSoundDirectory(directory: *const c_char) -> c_int;
}

struct RuntimeState {
    startup: Instant,
    key_queue: [u16; KEY_QUEUE_LEN],
    key_queue_write_idx: usize,
    key_queue_read_idx: usize,
    mouse_enabled: bool,
    last_mouse_position: Option<(u16, u16)>,
    mouse_buttons: c_int,
    rgba_frame: Vec<u8>,
    frame_sent_at: Option<Instant>,
    presentation: Option<Presentation>,
    fatal_error: Option<String>,
}

impl RuntimeState {
    fn new(presentation: Presentation) -> Self {
        Self {
            startup: Instant::now(),
            key_queue: [0; KEY_QUEUE_LEN],
            key_queue_write_idx: 0,
            key_queue_read_idx: 0,
            mouse_enabled: true,
            last_mouse_position: None,
            mouse_buttons: 0,
            rgba_frame: vec![0; RGBA_FRAME_BYTES],
            frame_sent_at: None,
            presentation: Some(presentation),
            fatal_error: None,
        }
    }

    fn enqueue_key(&mut self, pressed: bool, doom_key: u8) {
        let next = (self.key_queue_write_idx + 1) % KEY_QUEUE_LEN;
        if next == self.key_queue_read_idx {
            self.key_queue_read_idx = (self.key_queue_read_idx + 1) % KEY_QUEUE_LEN;
        }
        self.key_queue[self.key_queue_write_idx] = (u16::from(pressed) << 8) | u16::from(doom_key);
        self.key_queue_write_idx = next;
    }

    fn pop_key(&mut self) -> Option<(c_int, u8)> {
        if self.key_queue_read_idx == self.key_queue_write_idx {
            return None;
        }
        let key_data = self.key_queue[self.key_queue_read_idx];
        self.key_queue_read_idx = (self.key_queue_read_idx + 1) % KEY_QUEUE_LEN;
        Some((c_int::from(key_data >> 8), (key_data & 0xff) as u8))
    }

    fn fail(&mut self, error: impl std::fmt::Display) {
        if self.fatal_error.is_none() {
            self.fatal_error = Some(error.to_string());
        }
        request_exit();
    }

    fn poll_presentation(&mut self) {
        if let Some(presentation) = &mut self.presentation
            && let Err(error) = presentation.poll()
        {
            self.fail(error);
        }
    }
}

pub fn reset_exit_request() {
    EXIT_FLAG.store(false, Ordering::SeqCst);
}

pub fn request_exit() {
    EXIT_FLAG.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() -> Result<()> {
    if SIGNAL_HANDLERS_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(());
    }
    // SAFETY: the handler only performs an atomic store, which is async-signal-safe, and all
    // captured state is static. The registry keeps the handler installed after the ID is dropped.
    let interrupt_handler = match unsafe { signal_hook::low_level::register(SIGINT, request_exit) }
    {
        Ok(handler) => handler,
        Err(error) => {
            SIGNAL_HANDLERS_INSTALLED.store(false, Ordering::SeqCst);
            return Err(error).context("failed to install SIGINT handler");
        }
    };
    // SAFETY: this handler has the same static, atomic-only behavior as the SIGINT handler above.
    if let Err(error) = unsafe { signal_hook::low_level::register(SIGTERM, request_exit) } {
        signal_hook::low_level::unregister(interrupt_handler);
        SIGNAL_HANDLERS_INSTALLED.store(false, Ordering::SeqCst);
        return Err(error).context("failed to install SIGTERM handler");
    }
    Ok(())
}

pub fn run(
    c_args: &mut [CString],
    presentation: Presentation,
    sound_directory: &Path,
) -> Result<()> {
    if STATE
        .set(Mutex::new(RuntimeState::new(presentation)))
        .is_err()
    {
        bail!("Doom runtime has already been initialized");
    }
    let argc = c_int::try_from(c_args.len()).context("too many Doom arguments")?;
    let sound_directory = sound_directory_for_c(sound_directory);
    let sound_directory = CString::new(sound_directory.as_bytes())
        .context("sound directory contains an interior NUL byte")?;
    // SAFETY: the bridge copies the NUL-terminated directory before returning, and sound setup
    // happens before Doom or the mixer worker can read it.
    if unsafe { VVDOOM_SetSoundDirectory(sound_directory.as_ptr()) } == 0 {
        bail!("failed to allocate the sound directory path");
    }
    let mut argv: Vec<*mut c_char> = c_args
        .iter_mut()
        .map(|arg| arg.as_ptr().cast_mut())
        .collect();

    // SAFETY: every argv pointer references a live CString for the complete engine lifetime and
    // argc is the checked vector length.
    unsafe {
        doomgeneric_Create(argc, argv.as_mut_ptr());
    }
    // Doom writes a verbose startup transcript to the alternate-screen terminal. The Vivid
    // raster is composed with that terminal plane, so erase the transcript before activating the
    // media slots instead of leaving text over the game image.
    if let Err(error) = terminal::clear_for_presentation() {
        with_state_mut(|state| state.fail(error));
    }
    with_state_mut(|state| {
        if let Some(presentation) = &mut state.presentation
            && let Err(error) = presentation.start_workers()
        {
            state.fail(error);
        }
    });
    if !EXIT_FLAG.load(Ordering::SeqCst) {
        // One tick produces the recovery frame needed before slot activation.
        // SAFETY: doomgeneric_Create completed and the C engine remains single-threaded here.
        unsafe { doomgeneric_Tick() };
        with_state_mut(|state| {
            if let Some(presentation) = &mut state.presentation
                && let Err(error) = presentation.activate()
            {
                state.fail(error);
            }
        });
    }

    while !EXIT_FLAG.load(Ordering::SeqCst) {
        // SAFETY: all Doom engine calls stay on this thread after successful initialization.
        unsafe { doomgeneric_Tick() };
    }

    let (presentation, fatal_error) =
        with_state_mut(|state| (state.presentation.take(), state.fatal_error.take()))
            .unwrap_or((None, Some("Doom runtime state disappeared".into())));
    if let Some(presentation) = presentation {
        presentation.shutdown()?;
    }
    if let Some(error) = fatal_error {
        bail!(error);
    }
    Ok(())
}

fn sound_directory_for_c(path: &Path) -> Cow<'_, str> {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
            return Cow::Owned(format!(r"\\{path}"));
        }
        if let Some(path) = path.strip_prefix(r"\\?\") {
            return Cow::Owned(path.to_owned());
        }
    }
    path
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_Init() {}

#[unsafe(no_mangle)]
pub extern "C" fn DG_DrawFrame() {
    with_state_mut(|state| {
        drain_terminal_events(state);
        state.poll_presentation();
        let changed = convert_doom_pixels_from_global(&mut state.rgba_frame);
        // Doom draws twice per 35 Hz tic: once when TryRunTics returns early so the menu stays
        // responsive, and once after the tic it waited for actually runs. Submitting both halves
        // spends the whole declared 35 records/s budget on a stream that is half repeats, and the
        // raster worker then sits out a full frame period inside the rate limiter still holding
        // the frame it took before the wait — so every image reaches the wire about one frame
        // after a newer one already existed. Submitting only what changed keeps production at the
        // declared rate, which leaves the limiter with capacity in hand and puts each new frame
        // on the wire as soon as Doom draws it.
        let refresh_due = state
            .frame_sent_at
            .is_none_or(|sent| sent.elapsed() >= FRAME_REFRESH_INTERVAL);
        if changed || refresh_due {
            if let Some(presentation) = &state.presentation
                && let Err(error) = presentation.submit_frame(&state.rgba_frame)
            {
                state.fail(error);
            }
            state.frame_sent_at = Some(Instant::now());
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_SleepMs(ms: c_uint) {
    thread::sleep(Duration::from_millis(u64::from(ms)));
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_GetTicksMs() -> u32 {
    with_state_mut(|state| u32::try_from(state.startup.elapsed().as_millis()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_GetKey(pressed: *mut c_int, doom_key: *mut u8) -> c_int {
    with_state_mut(|state| {
        drain_terminal_events(state);
        state.poll_presentation();
        if let Some((is_pressed, key)) = state.pop_key() {
            // SAFETY: Doom supplies writable pointers when it wants each output; null means omit.
            unsafe {
                if !pressed.is_null() {
                    *pressed = is_pressed;
                }
                if !doom_key.is_null() {
                    *doom_key = key;
                }
            }
            1
        } else {
            0
        }
    })
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_SetWindowTitle(title: *const c_char) {
    if title.is_null() {
        return;
    }
    // SAFETY: Doom owns a NUL-terminated title for the duration of this callback.
    let Ok(title) = unsafe { CStr::from_ptr(title) }.to_str() else {
        return;
    };
    let mut stdout = io::stdout();
    let _ = write!(stdout, "\x1b]0;{title}\x07");
    let _ = stdout.flush();
}

#[unsafe(no_mangle)]
pub extern "C" fn DG_ErrorExit() {
    request_exit();
    terminal::restore_terminal();
}

fn with_state_mut<T>(f: impl FnOnce(&mut RuntimeState) -> T) -> Option<T> {
    let state = STATE.get()?;
    let mut guard = state.lock().ok()?;
    Some(f(&mut guard))
}

fn drain_terminal_events(state: &mut RuntimeState) {
    while let Ok(true) = event::poll(Duration::ZERO) {
        match event::read() {
            Ok(Event::Key(key)) => handle_key_event(state, key),
            Ok(Event::Mouse(mouse)) => handle_mouse_event(state, mouse),
            Ok(Event::Resize(_, _)) => state.last_mouse_position = None,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn handle_key_event(state: &mut RuntimeState, key: KeyEvent) {
    let pressed = matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
    if is_interrupt_key(&key) {
        request_exit();
        return;
    }
    match key.code {
        KeyCode::Char('m' | 'M') => {
            if key.kind == KeyEventKind::Release {
                state.mouse_enabled = !state.mouse_enabled;
                state.last_mouse_position = None;
            }
            return;
        }
        KeyCode::Char('u' | 'U') => {
            if key.kind == KeyEventKind::Release {
                state.last_mouse_position = None;
                if let Some(presentation) = &mut state.presentation
                    && let Err(error) = presentation.toggle_scale()
                {
                    state.fail(error);
                }
            }
            return;
        }
        _ => {}
    }
    if let Some(doom_key) = input::doom_key_for(key) {
        state.enqueue_key(pressed, doom_key);
    }
}

fn is_interrupt_key(key: &KeyEvent) -> bool {
    let pressed = matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
    let ctrl_c = matches!(key.code, KeyCode::Char('c' | 'C'))
        && key.modifiers.contains(KeyModifiers::CONTROL);
    let raw_etx = matches!(key.code, KeyCode::Char('\u{3}'));
    pressed && (ctrl_c || raw_etx)
}

fn handle_mouse_event(state: &mut RuntimeState, mouse: crossterm::event::MouseEvent) {
    if !state.mouse_enabled {
        state.last_mouse_position = None;
        return;
    }
    let Some(presentation) = &state.presentation else {
        return;
    };
    let current = (mouse.column, mouse.row);
    let (rel_x, rel_y) = state
        .last_mouse_position
        .map(|previous| presentation.layout().mouse_delta(previous, current))
        .unwrap_or((0, 0));
    state.last_mouse_position = Some(current);

    match mouse.kind {
        MouseEventKind::Down(button) | MouseEventKind::Drag(button) => {
            state.mouse_buttons |= mouse_button_bit(button);
        }
        MouseEventKind::Up(button) => state.mouse_buttons &= !mouse_button_bit(button),
        MouseEventKind::Moved => {}
        MouseEventKind::ScrollDown
        | MouseEventKind::ScrollUp
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => return,
    }

    let mut event = DoomEvent {
        event_type: DoomEventType::Mouse,
        data1: state.mouse_buttons,
        data2: accelerate_mouse(rel_x, 16.0),
        data3: -accelerate_mouse(rel_y, 4.0),
        data4: 0,
    };
    // SAFETY: D_PostEvent copies the stack event synchronously during the callback.
    unsafe { D_PostEvent(ptr::addr_of_mut!(event)) };
}

fn mouse_button_bit(button: MouseButton) -> c_int {
    match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
    }
}

fn accelerate_mouse(delta: c_int, clamp: f32) -> c_int {
    let dx = delta as f32;
    (dx * clamp.min(8.0 * dx.abs().exp())) as c_int
}

fn convert_doom_pixels_from_global(output: &mut [u8]) -> bool {
    // SAFETY: Doom owns a 640x400 u32 framebuffer from initialization through shutdown. A null
    // pointer means no frame is available yet.
    let screen = unsafe {
        let pointer = DG_ScreenBuffer;
        if pointer.is_null() {
            return false;
        }
        std::slice::from_raw_parts(pointer, DOOM_PIXELS)
    };
    convert_doom_pixels(screen, output)
}

/// Convert Doom's packed framebuffer into `output`, reporting whether the image changed.
///
/// `output` holds the previously converted frame, so the comparison is the write it was already
/// going to do rather than a second pass over a megabyte.
pub fn convert_doom_pixels(input: &[u32], output: &mut [u8]) -> bool {
    let mut changed = false;
    let (pixels, _) = output.as_chunks_mut::<4>();
    for (pixel, rgba) in input.iter().zip(pixels) {
        let converted = [
            ((pixel >> 16) & 0xff) as u8,
            ((pixel >> 8) & 0xff) as u8,
            (pixel & 0xff) as u8,
            u8::MAX,
        ];
        changed |= *rgba != converted;
        *rgba = converted;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    use vivid_sdk::testing::{ROOT_SECRET_HEX, TestPresenter};
    use vivid_sdk::{ProducerAuthentication, ProducerConfig, Session};

    #[test]
    fn converts_packed_doom_pixels_to_rgba() {
        let input = [0x0012_3456, 0x00ab_cdef];
        let mut output = [0; 8];
        assert!(convert_doom_pixels(&input, &mut output));
        assert_eq!(output, [0x12, 0x34, 0x56, 0xff, 0xab, 0xcd, 0xef, 0xff]);
    }

    #[test]
    fn reconverting_the_same_framebuffer_reports_no_change() {
        let input = [0x0012_3456, 0x00ab_cdef];
        let mut output = [0; 8];
        assert!(convert_doom_pixels(&input, &mut output));
        assert!(!convert_doom_pixels(&input, &mut output));
        let moved = [0x0012_3456, 0x00ab_cdee];
        assert!(convert_doom_pixels(&moved, &mut output));
        assert_eq!(output[4..], [0xab, 0xcd, 0xee, 0xff]);
    }

    #[test]
    fn full_key_queue_drops_oldest_without_appearing_empty() {
        // RuntimeState requires a live presentation, so exercise the ring arithmetic directly.
        let mut write = 0;
        let mut read = 0;
        for _ in 0..KEY_QUEUE_LEN {
            let next = (write + 1) % KEY_QUEUE_LEN;
            if next == read {
                read = (read + 1) % KEY_QUEUE_LEN;
            }
            write = next;
        }
        assert_ne!(read, write);
    }

    #[test]
    fn detects_control_c_interrupt() {
        assert!(is_interrupt_key(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(!is_interrupt_key(&KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        )));
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_verbatim_windows_sound_paths_for_miniaudio() {
        assert_eq!(
            sound_directory_for_c(Path::new(r"\\?\C:\games\vvdoom\sound")),
            r"C:\games\vvdoom\sound"
        );
        assert_eq!(
            sound_directory_for_c(Path::new(r"\\?\UNC\server\share\sound")),
            r"\\server\share\sound"
        );
    }

    #[test]
    fn doom_engine_emits_nonzero_headless_pcm() {
        let presenter = TestPresenter::start(80, 24).unwrap();
        let endpoint = presenter.endpoint().to_owned();
        let session = Session::connect(ProducerConfig {
            endpoint_control: Some(endpoint.clone()),
            endpoint_realtime: Some(endpoint.clone()),
            endpoint_bulk: Some(endpoint),
            authentication: ProducerAuthentication::root_hex(ROOT_SECRET_HEX).unwrap(),
            producer_name: "vvdoom-engine-audio-test".into(),
            producer_version: env!("CARGO_PKG_VERSION").into(),
            ..ProducerConfig::default()
        })
        .unwrap();
        let presentation = Presentation::from_session(session, true, false).unwrap();
        let iwad = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("doom1.wad");
        let sound_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join("sound")
            .canonicalize()
            .unwrap();
        let mut arguments = [
            CString::new("vvdoom-test").unwrap(),
            CString::new("-iwad").unwrap(),
            CString::new(iwad.to_string_lossy().as_bytes()).unwrap(),
            CString::new("-warp").unwrap(),
            CString::new("1").unwrap(),
            CString::new("1").unwrap(),
        ];
        reset_exit_request();
        let stopper = thread::spawn(|| {
            thread::sleep(Duration::from_secs(2));
            request_exit();
        });
        run(&mut arguments, presentation, &sound_dir).unwrap();
        stopper.join().unwrap();
        assert!(
            crate::media::nonzero_mixer_samples() > 0,
            "Doom and miniaudio produced only silence"
        );
    }
}
