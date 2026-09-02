use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, ModifierKeyCode};

pub const KEY_RIGHTARROW: u8 = 0xae;
pub const KEY_LEFTARROW: u8 = 0xac;
pub const KEY_UPARROW: u8 = 0xad;
pub const KEY_DOWNARROW: u8 = 0xaf;
pub const KEY_STRAFE_L: u8 = 0xa0;
pub const KEY_STRAFE_R: u8 = 0xa1;
pub const KEY_USE: u8 = 0xa2;
pub const KEY_FIRE: u8 = 0xa3;
pub const KEY_ESCAPE: u8 = 27;
pub const KEY_ENTER: u8 = 13;
pub const KEY_TAB: u8 = 9;
pub const KEY_F1: u8 = 0x80 + 0x3b;
pub const KEY_F2: u8 = 0x80 + 0x3c;
pub const KEY_F3: u8 = 0x80 + 0x3d;
pub const KEY_F4: u8 = 0x80 + 0x3e;
pub const KEY_F5: u8 = 0x80 + 0x3f;
pub const KEY_F6: u8 = 0x80 + 0x40;
pub const KEY_F7: u8 = 0x80 + 0x41;
pub const KEY_F8: u8 = 0x80 + 0x42;
pub const KEY_F9: u8 = 0x80 + 0x43;
pub const KEY_F10: u8 = 0x80 + 0x44;
pub const KEY_F11: u8 = 0x80 + 0x57;
pub const KEY_F12: u8 = 0x80 + 0x58;
pub const KEY_BACKSPACE: u8 = 0x7f;
pub const KEY_EQUALS: u8 = 0x3d;
pub const KEY_MINUS: u8 = 0x2d;
pub const KEY_RSHIFT: u8 = 0x80 + 0x36;
pub const KEY_RALT: u8 = 0x80 + 0x38;
pub const KEY_LALT: u8 = KEY_RALT;

pub fn doom_key_for(key: KeyEvent) -> Option<u8> {
    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            KeyCode::Left => return Some(KEY_STRAFE_L),
            KeyCode::Right => return Some(KEY_STRAFE_R),
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter => Some(KEY_ENTER),
        KeyCode::Esc => Some(KEY_ESCAPE),
        KeyCode::Tab => Some(KEY_TAB),
        KeyCode::Backspace => Some(KEY_BACKSPACE),
        KeyCode::Left => Some(KEY_LEFTARROW),
        KeyCode::Right => Some(KEY_RIGHTARROW),
        KeyCode::Up => Some(KEY_UPARROW),
        KeyCode::Down => Some(KEY_DOWNARROW),
        KeyCode::F(1) => Some(KEY_F1),
        KeyCode::F(2) => Some(KEY_F2),
        KeyCode::F(3) => Some(KEY_F3),
        KeyCode::F(4) => Some(KEY_F4),
        KeyCode::F(5) => Some(KEY_F5),
        KeyCode::F(6) => Some(KEY_F6),
        KeyCode::F(7) => Some(KEY_F7),
        KeyCode::F(8) => Some(KEY_F8),
        KeyCode::F(9) => Some(KEY_F9),
        KeyCode::F(10) => Some(KEY_F10),
        KeyCode::F(11) => Some(KEY_F11),
        KeyCode::F(12) => Some(KEY_F12),
        KeyCode::Modifier(ModifierKeyCode::LeftControl)
        | KeyCode::Modifier(ModifierKeyCode::RightControl) => Some(KEY_FIRE),
        KeyCode::Modifier(ModifierKeyCode::LeftAlt)
        | KeyCode::Modifier(ModifierKeyCode::RightAlt) => Some(KEY_LALT),
        KeyCode::Modifier(ModifierKeyCode::LeftShift)
        | KeyCode::Modifier(ModifierKeyCode::RightShift) => Some(KEY_RSHIFT),
        KeyCode::Char(ch) => char_to_doom_key(ch),
        _ => None,
    }
}

fn char_to_doom_key(ch: char) -> Option<u8> {
    let ch = ch.to_ascii_lowercase();
    match ch {
        'j' => Some(KEY_LEFTARROW),
        'l' => Some(KEY_RIGHTARROW),
        'w' => Some(KEY_UPARROW),
        'k' | 's' => Some(KEY_DOWNARROW),
        'f' | 'i' => Some(KEY_FIRE),
        ' ' => Some(KEY_USE),
        'a' => Some(KEY_STRAFE_L),
        'd' => Some(KEY_STRAFE_R),
        '=' | '+' => Some(KEY_EQUALS),
        '-' => Some(KEY_MINUS),
        ch if ch.is_ascii() => Some(ch as u8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_movement_keys() {
        assert_eq!(
            doom_key_for(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE)),
            Some(KEY_UPARROW)
        );
        assert_eq!(
            doom_key_for(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some(KEY_STRAFE_L)
        );
        assert_eq!(
            doom_key_for(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            Some(KEY_STRAFE_R)
        );
        assert_eq!(
            doom_key_for(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)),
            Some(KEY_FIRE)
        );
    }

    #[test]
    fn maps_alt_arrows_to_strafe() {
        assert_eq!(
            doom_key_for(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT)),
            Some(KEY_STRAFE_L)
        );
        assert_eq!(
            doom_key_for(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT)),
            Some(KEY_STRAFE_R)
        );
    }
}
