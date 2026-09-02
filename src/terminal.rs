use std::{
    io::{self, Write},
    panic,
};

use anyhow::Result;
use crossterm::{
    Command,
    cursor::{Hide, MoveTo, Show},
    event::{DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags},
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode,
    },
};

pub struct TerminalSession;

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            PushKeyboardEnhancement(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
            ),
            EnableMouseCapture,
            EnableSgrPixelMouse,
            Hide,
            Clear(ClearType::All),
            MoveTo(0, 0)
        ) {
            restore_terminal();
            return Err(error.into());
        }
        stdout.flush()?;
        Ok(Self)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        restore_terminal();
    }
}

pub fn clear_for_presentation() -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, Clear(ClearType::All), MoveTo(0, 0))?;
    stdout.flush()
}

pub fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        Show,
        DisableSgrPixelMouse,
        DisableMouseCapture,
        PopKeyboardEnhancement,
        LeaveAlternateScreen,
        MoveTo(0, 0)
    );
    let _ = stdout.flush();
    let _ = disable_raw_mode();
}

pub fn install_panic_restore_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EnableSgrPixelMouse;

impl Command for EnableSgrPixelMouse {
    fn write_ansi(&self, formatter: &mut impl std::fmt::Write) -> std::fmt::Result {
        formatter.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h\x1b[?1016h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SGR pixel mouse mode requires ANSI escape support",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisableSgrPixelMouse;

impl Command for DisableSgrPixelMouse {
    fn write_ansi(&self, formatter: &mut impl std::fmt::Write) -> std::fmt::Result {
        formatter.write_str("\x1b[?1016l\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SGR pixel mouse mode requires ANSI escape support",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PushKeyboardEnhancement(KeyboardEnhancementFlags);

impl Command for PushKeyboardEnhancement {
    fn write_ansi(&self, formatter: &mut impl std::fmt::Write) -> std::fmt::Result {
        if cfg!(windows) {
            Ok(())
        } else {
            write!(formatter, "\x1b[>{}u", self.0.bits())
        }
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keyboard enhancement requires ANSI escape support",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PopKeyboardEnhancement;

impl Command for PopKeyboardEnhancement {
    fn write_ansi(&self, formatter: &mut impl std::fmt::Write) -> std::fmt::Result {
        if cfg!(windows) {
            Ok(())
        } else {
            formatter.write_str("\x1b[<1u")
        }
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keyboard enhancement requires ANSI escape support",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}
