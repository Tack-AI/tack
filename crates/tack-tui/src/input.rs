//! Input event model. Decoupled from crossterm so tests can inject events
//! directly; `terminal.rs` does the crossterm → InputEvent mapping.

/// A keyboard/mouse/paste event delivered to the focused component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Key(KeyEvent),
    /// Bracketed paste payload (raw text; may be multi-line).
    Paste(String),
    Mouse(MouseEvent),
    Resize {
        width: u16,
        height: u16,
    },
    FocusGained,
    FocusLost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub modifiers: Modifiers,
    /// Key release events exist only with the Kitty keyboard protocol.
    pub is_release: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: false,
    };
    pub const CTRL: Modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: false,
    };
    pub const ALT: Modifiers = Modifiers {
        ctrl: false,
        alt: true,
        shift: false,
    };
    pub const SHIFT: Modifiers = Modifiers {
        ctrl: false,
        alt: false,
        shift: true,
    };
    pub const CTRL_ALT: Modifiers = Modifiers {
        ctrl: true,
        alt: true,
        shift: false,
    };
    pub const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        alt: false,
        shift: true,
    };
    pub const ALT_SHIFT: Modifiers = Modifiers {
        ctrl: false,
        alt: true,
        shift: true,
    };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    Insert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MouseEventKind {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    ScrollUp,
    ScrollDown,
    /// Wheel left/right (horizontal scroll).
    ScrollLeft,
    ScrollRight,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub column: u16,
    pub row: u16,
    pub kind: MouseEventKind,
    pub modifiers: Modifiers,
}

impl KeyEvent {
    pub fn new(key: Key, modifiers: Modifiers) -> Self {
        KeyEvent {
            key,
            modifiers,
            is_release: false,
        }
    }

    pub fn plain(key: Key) -> Self {
        KeyEvent::new(key, Modifiers::NONE)
    }

    pub fn ctrl(key: Key) -> Self {
        KeyEvent::new(key, Modifiers::CTRL)
    }

    /// Port of tack-tui's `matchesKey`: match against a spec like
    /// "ctrl+shift+up" / "alt+enter" / "escape" / a bare character.
    pub fn matches(&self, spec: &str) -> bool {
        if self.is_release {
            return false;
        }
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        // Spec words are split on '+'; the last token is the key.
        let tokens: Vec<&str> = spec.split('+').collect();
        let (mods, key) = tokens.split_at(tokens.len().saturating_sub(1));
        for m in mods {
            match m.to_ascii_lowercase().as_str() {
                "ctrl" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                _ => return false,
            }
        }
        let key_part = key.first().copied().unwrap_or(spec);
        self.modifiers.ctrl == ctrl
            && self.modifiers.alt == alt
            && self.modifiers.shift == shift
            && self.key_matches(key_part)
    }

    fn key_matches(&self, spec: &str) -> bool {
        let lower = spec.to_ascii_lowercase();
        match lower.as_str() {
            "enter" => self.key == Key::Enter,
            "escape" => self.key == Key::Escape,
            "backspace" => self.key == Key::Backspace,
            "delete" => self.key == Key::Delete,
            "tab" => {
                // Crossterm reports shift+tab as BackTab + SHIFT; "shift+tab"
                // specs must match it (the SHIFT modifier is still verified
                // by the caller, so plain "tab" never matches BackTab).
                self.key == Key::Tab || self.key == Key::BackTab
            }
            "backtab" => self.key == Key::BackTab,
            "up" => self.key == Key::Up,
            "down" => self.key == Key::Down,
            "left" => self.key == Key::Left,
            "right" => self.key == Key::Right,
            "home" => self.key == Key::Home,
            "end" => self.key == Key::End,
            "pageup" => self.key == Key::PageUp,
            "pagedown" => self.key == Key::PageDown,
            "insert" => self.key == Key::Insert,
            other => {
                if let Some(f) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                    && (1..=12).contains(&f)
                {
                    return self.key == Key::F(f);
                }
                // Single character. The spec side is lowercased above and
                // cannot express case, so compare case-insensitively —
                // terminals report shift+a as Char('A') + shift, and
                // "shift+a" must still match.
                let mut chars = other.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => matches!(
                        self.key,
                        Key::Char(actual) if actual.eq_ignore_ascii_case(&c)
                    ),
                    _ => false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_specs() {
        assert!(KeyEvent::ctrl(Key::Char('c')).matches("ctrl+c"));
        assert!(KeyEvent::plain(Key::Enter).matches("enter"));
        assert!(KeyEvent::new(Key::Up, Modifiers::CTRL_SHIFT).matches("ctrl+shift+up"));
        assert!(!KeyEvent::new(Key::Up, Modifiers::CTRL).matches("ctrl+shift+up"));
        assert!(KeyEvent::plain(Key::Char('a')).matches("a"));
        assert!(!KeyEvent::plain(Key::Char('a')).matches("ctrl+a"));
        assert!(!KeyEvent::ctrl(Key::Char('c')).matches("c"));
        // Release events never match.
        let release = KeyEvent {
            key: Key::Enter,
            modifiers: Modifiers::NONE,
            is_release: true,
        };
        assert!(!release.matches("enter"));
    }

    #[test]
    fn shift_tab_matches_backtab_events() {
        // Terminals report shift+tab as BackTab + shift (crossterm maps
        // KeyCode::BackTab with the SHIFT modifier); key_matches("tab")
        // only accepted Key::Tab, so every "shift+tab" binding was dead.
        assert!(KeyEvent::new(Key::BackTab, Modifiers::SHIFT).matches("shift+tab"));
        assert!(!KeyEvent::new(Key::BackTab, Modifiers::NONE).matches("shift+tab"));
        assert!(!KeyEvent::new(Key::Tab, Modifiers::NONE).matches("shift+tab"));
        // The explicit "backtab" spec matches a modifier-less BackTab event
        // (spec modifiers must equal event modifiers, like every other key).
        assert!(KeyEvent::new(Key::BackTab, Modifiers::NONE).matches("backtab"));
        assert!(KeyEvent::new(Key::Tab, Modifiers::NONE).matches("tab"));
    }

    #[test]
    fn shift_letter_specs_match_uppercase_chars() {
        // Regression: terminals report shift+a as Char('A') + shift; the
        // lowercased spec 'a' never equaled 'A', so every shift+letter
        // binding was dead.
        assert!(KeyEvent::new(Key::Char('A'), Modifiers::SHIFT).matches("shift+a"));
        assert!(KeyEvent::new(Key::Char('C'), Modifiers::CTRL_SHIFT).matches("ctrl+shift+c"));
        // Case folding must not make wrong keys match.
        assert!(!KeyEvent::new(Key::Char('B'), Modifiers::SHIFT).matches("shift+a"));
    }
}
