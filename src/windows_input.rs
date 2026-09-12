//! ConPTY input is a VT stream, not a stream of physical Win32 key transitions.
//! ReadConsoleInput (used by crossterm on Windows) exposes synthesized key-up events
//! and cannot decode Kitty releases or SGR pixel mouse reports. Read VT characters
//! instead, preserving the enhanced events requested from Vivido.

use std::{
    io,
    os::windows::io::AsRawHandle,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode, MouseButton, MouseEvent,
    MouseEventKind,
};
use windows_sys::Win32::System::{
    Console::{
        ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle,
        ReadConsoleW, STD_INPUT_HANDLE, SetConsoleMode,
    },
    IO::CancelSynchronousIo,
};

static INPUT: Mutex<Option<Input>> = Mutex::new(None);

struct Input {
    events: Option<Receiver<io::Result<Event>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    mode: u32,
}

pub fn start() -> io::Result<()> {
    let mut input = INPUT
        .lock()
        .map_err(|_| io::Error::other("input lock poisoned"))?;
    if input.is_some() {
        return Err(io::Error::other("terminal input already active"));
    }
    // SAFETY: the process owns stdin; GetConsoleMode validates that it is a console.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0;
    // SAFETY: mode points to a writable DWORD and handle is used only by console APIs.
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let raw_mode = (mode | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_EXTENDED_FLAGS)
        & !(ENABLE_ECHO_INPUT
            | ENABLE_LINE_INPUT
            | ENABLE_PROCESSED_INPUT
            | ENABLE_QUICK_EDIT_MODE);
    // SAFETY: handle is a console input handle, validated above.
    if unsafe { SetConsoleMode(handle, raw_mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (sender, events) = mpsc::sync_channel(128);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let worker = thread::Builder::new()
        .name("vvdoom-input".into())
        .spawn(move || {
            // SAFETY: stdin remains open for the lifetime of this worker.
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            let mut parser = Parser::default();
            let mut buffer = [0u16; 128];
            while !stopped.load(Ordering::Acquire) {
                let mut count = 0;
                // SAFETY: buffer has the declared number of writable UTF-16 units; count is
                // writable, no cooked-read control is supplied, and the call is synchronous.
                let ok = unsafe {
                    ReadConsoleW(
                        handle,
                        buffer.as_mut_ptr().cast(),
                        buffer.len() as u32,
                        &mut count,
                        std::ptr::null(),
                    )
                };
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                if ok == 0 || count == 0 {
                    let error = if ok == 0 {
                        io::Error::last_os_error()
                    } else {
                        io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")
                    };
                    let _ = sender.send(Err(error));
                    break;
                }
                for &unit in &buffer[..count as usize] {
                    if let Some(event) = parser.feed(unit)
                        && sender.send(Ok(event)).is_err()
                    {
                        return;
                    }
                }
            }
        });
    match worker {
        Ok(worker) => {
            *input = Some(Input {
                events: Some(events),
                stop,
                worker: Some(worker),
                mode,
            })
        }
        Err(error) => {
            // SAFETY: restore the mode read from this same console before spawning.
            unsafe {
                SetConsoleMode(handle, mode);
            }
            return Err(error);
        }
    }
    Ok(())
}

pub fn read_event() -> io::Result<Option<Event>> {
    let input = INPUT
        .lock()
        .map_err(|_| io::Error::other("input lock poisoned"))?;
    let Some(events) = input.as_ref().and_then(|input| input.events.as_ref()) else {
        return Ok(None);
    };
    match events.try_recv() {
        Ok(event) => event.map(Some),
        Err(TryRecvError::Empty) => Ok(None),
        Err(TryRecvError::Disconnected) => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "terminal input worker stopped",
        )),
    }
}

pub fn stop() {
    if let Ok(mut input) = INPUT.lock() {
        input.take();
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake a sender blocked on the bounded queue before cancelling a console read.
        self.events.take();
        if let Some(worker) = self.worker.take() {
            while !worker.is_finished() {
                // SAFETY: JoinHandle keeps the worker's thread handle alive. Repeating the
                // cancellation closes the race between its stop check and entering ReadConsole.
                unsafe {
                    CancelSynchronousIo(worker.as_raw_handle());
                }
                thread::sleep(Duration::from_millis(1));
            }
            let _ = worker.join();
        }
        // SAFETY: the reader has stopped and stdin still belongs to the process.
        unsafe {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), self.mode);
        }
    }
}

/// Only the bounded ASCII control sequences requested by vvdoom are needed. Game
/// bindings are ASCII; non-ASCII text is ignored, never interpreted as control bytes.
#[derive(Default)]
struct Parser {
    sequence: String,
    escaped: bool,
    csi: bool,
    overflow: bool,
}

impl Parser {
    fn feed(&mut self, unit: u16) -> Option<Event> {
        if unit == 27 {
            self.sequence.clear();
            self.escaped = true;
            self.csi = false;
            self.overflow = false;
            return None;
        }
        if self.escaped && !self.csi {
            self.csi = unit == u16::from(b'[') || unit == u16::from(b'O');
            self.escaped = self.csi;
            return None;
        }
        if !self.csi {
            // Enhanced mode encodes Escape as CSI 27u, so an isolated ESC never
            // needs a timeout that would delay the next key.
            let code = match unit {
                3 => KeyCode::Char('\u{3}'),
                9 => KeyCode::Tab,
                13 => KeyCode::Enter,
                127 => KeyCode::Backspace,
                32..=126 => KeyCode::Char(char::from_u32(u32::from(unit))?),
                _ => return None,
            };
            return Some(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        }
        if (0x40..=0x7e).contains(&unit) {
            self.csi = false;
            self.escaped = false;
            let event = (!self.overflow)
                .then(|| parse_csi(&self.sequence, unit as u8))
                .flatten();
            self.sequence.clear();
            return event;
        }
        if self.sequence.len() == 96 || !(0x20..=0x3f).contains(&unit) {
            self.overflow = true;
        }
        if !self.overflow {
            self.sequence.push(unit as u8 as char);
        }
        None
    }
}

fn parse_csi(body: &str, final_byte: u8) -> Option<Event> {
    if let Some(mouse) = body.strip_prefix('<') {
        return parse_mouse(mouse, final_byte).map(Event::Mouse);
    }
    let mut fields = body.split(';');
    let number = fields.next()?.split(':').next()?;
    let number = if number.is_empty() {
        1
    } else {
        number.parse::<u32>().ok()?
    };
    let mut modifiers = fields.next().unwrap_or("1").split(':');
    let bits = modifiers.next()?.parse::<u8>().ok()?.checked_sub(1)?;
    let kind = match modifiers.next().unwrap_or("1") {
        "1" => KeyEventKind::Press,
        "2" => KeyEventKind::Repeat,
        "3" => KeyEventKind::Release,
        _ => return None,
    };
    let code = match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P'..=b'S' => KeyCode::F(final_byte - b'P' + 1),
        b'~' => match number {
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            11..=15 => KeyCode::F((number - 10) as u8),
            17..=21 => KeyCode::F((number - 11) as u8),
            23..=24 => KeyCode::F((number - 12) as u8),
            _ => return None,
        },
        b'u' => match number {
            9 => KeyCode::Tab,
            13 => KeyCode::Enter,
            27 => KeyCode::Esc,
            127 => KeyCode::Backspace,
            57417 => KeyCode::Left,
            57418 => KeyCode::Right,
            57419 => KeyCode::Up,
            57420 => KeyCode::Down,
            57441 => KeyCode::Modifier(ModifierKeyCode::LeftShift),
            57442 => KeyCode::Modifier(ModifierKeyCode::LeftControl),
            57443 => KeyCode::Modifier(ModifierKeyCode::LeftAlt),
            57447 => KeyCode::Modifier(ModifierKeyCode::RightShift),
            57448 => KeyCode::Modifier(ModifierKeyCode::RightControl),
            57449 => KeyCode::Modifier(ModifierKeyCode::RightAlt),
            32..=126 => KeyCode::Char(char::from_u32(number)?),
            _ => return None,
        },
        _ => return None,
    };
    let mut flags = KeyModifiers::NONE;
    flags.set(KeyModifiers::SHIFT, bits & 1 != 0);
    flags.set(KeyModifiers::ALT, bits & 2 != 0);
    flags.set(KeyModifiers::CONTROL, bits & 4 != 0);
    Some(Event::Key(KeyEvent::new_with_kind(code, flags, kind)))
}

fn parse_mouse(body: &str, final_byte: u8) -> Option<MouseEvent> {
    if !matches!(final_byte, b'M' | b'm') {
        return None;
    }
    let mut fields = body.split(';');
    let bits = fields.next()?.parse::<u8>().ok()?;
    let column = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let row = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    if fields.next().is_some() {
        return None;
    }
    let button = match bits & 3 {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        _ => None,
    };
    let kind = if bits & 64 != 0 {
        match bits & 3 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if final_byte == b'm' {
        MouseEventKind::Up(button?)
    } else if bits & 32 != 0 {
        button.map_or(MouseEventKind::Moved, MouseEventKind::Drag)
    } else {
        MouseEventKind::Down(button?)
    };
    let mut modifiers = KeyModifiers::NONE;
    modifiers.set(KeyModifiers::SHIFT, bits & 4 != 0);
    modifiers.set(KeyModifiers::ALT, bits & 8 != 0);
    modifiers.set(KeyModifiers::CONTROL, bits & 16 != 0);
    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Driven by tests/windows_input.py in a fresh ConPTY, never the user's console.
    #[test]
    #[ignore = "requires the ConPTY driver: python tests/windows_input.py"]
    fn conpty_input_probe() {
        use std::io::Write;
        use std::time::Instant;
        // SAFETY: probe runs in its own console; this only reads its input mode.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut original = 0;
        assert_ne!(unsafe { GetConsoleMode(handle, &mut original) }, 0);
        let terminal = crate::terminal::TerminalSession::enter().unwrap();
        println!("INPUT_READY");
        io::stdout().flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = Vec::new();
        while received.len() < 24 && Instant::now() < deadline {
            if let Some(event) = read_event().unwrap() {
                received.push(event);
                println!("INPUT_EVENT {}", received.len());
                io::stdout().flush().unwrap();
            }
            thread::sleep(Duration::from_millis(1));
        }
        let expected = [
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char('a'),
            KeyCode::Char('d'),
            KeyCode::Char('w'),
            KeyCode::Char('s'),
            KeyCode::Char('w'),
            KeyCode::Char('s'),
        ]
        .into_iter()
        .flat_map(|code| {
            [KeyEventKind::Press, KeyEventKind::Release]
                .map(|kind| Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind)))
        })
        .collect::<Vec<_>>();
        assert_eq!(received, expected);
        let before_stop = Instant::now();
        drop(terminal);
        assert!(
            before_stop.elapsed() < Duration::from_secs(1),
            "reader did not stop promptly"
        );
        let mut restored = 0;
        // SAFETY: same live console handle and writable mode output.
        assert_ne!(unsafe { GetConsoleMode(handle, &mut restored) }, 0);
        assert_eq!(original, restored);
        println!("INPUT_VERIFIED");
    }

    fn parse(text: &str) -> Vec<Event> {
        let mut parser = Parser::default();
        text.encode_utf16()
            .filter_map(|unit| parser.feed(unit))
            .collect()
    }

    #[test]
    fn held_arrows_and_wasd_preserve_real_releases() {
        for (sequence, code) in [
            ("1", KeyCode::Left),
            ("119", KeyCode::Char('w')),
            ("97", KeyCode::Char('a')),
            ("115", KeyCode::Char('s')),
            ("100", KeyCode::Char('d')),
        ] {
            let end = if code == KeyCode::Left { 'D' } else { 'u' };
            let events = parse(&format!(
                "\x1b[{sequence};1{end}\x1b[{sequence};1:2{end}\x1b[{sequence};1:3{end}"
            ));
            assert_eq!(
                events,
                [
                    KeyEventKind::Press,
                    KeyEventKind::Repeat,
                    KeyEventKind::Release
                ]
                .map(|kind| Event::Key(KeyEvent::new_with_kind(
                    code,
                    KeyModifiers::NONE,
                    kind
                )))
            );
        }
        assert_eq!(parse("\x1b[1;1C\x1b[1;1:3C").len(), 2);
    }

    #[test]
    fn fragmented_input_never_emits_sequence_characters_as_keys() {
        let mut parser = Parser::default();
        for unit in "\x1b[119;1:".encode_utf16() {
            assert!(parser.feed(unit).is_none());
        }
        assert!(parser.feed(u16::from(b'3')).is_none());
        assert_eq!(
            parser.feed(u16::from(b'u')),
            Some(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('w'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )))
        );
    }

    #[test]
    fn malformed_and_overlong_reports_are_bounded_and_recover() {
        let mut parser = Parser::default();
        for unit in format!("\x1b[{}u", "9".repeat(4096)).encode_utf16() {
            assert!(parser.feed(unit).is_none());
            assert!(parser.sequence.len() <= 96);
        }
        assert_eq!(
            parse("\x1b[<0;0;1M\x1b[1;0D\x1b[119;1:9u\x1b[27u"),
            [Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))]
        );
    }

    #[test]
    fn decodes_pixel_mouse_and_control_keys() {
        assert_eq!(
            parse("\x1b[<32;640;400M"),
            [Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 639,
                row: 399,
                modifiers: KeyModifiers::NONE,
            })]
        );
        assert_eq!(
            parse("\x1b[99;5u"),
            [Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))]
        );
        assert_eq!(
            parse("\x1b[1;3D"),
            [Event::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT))]
        );
    }
}
