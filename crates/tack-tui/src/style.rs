//! Terminal styling: colors and SGR attributes, emitted as ANSI only at the
//! output boundary (components work with styled spans, never raw ANSI).

/// A terminal color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    Default,
    /// 256-palette index.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// Parse a theme color value: hex string ("#rrggbb") or 256-palette int.
    pub fn parse(value: &serde_json::Value) -> Option<Color> {
        match value {
            serde_json::Value::String(s) => {
                let s = s.trim();
                let hex = s.strip_prefix('#')?;
                // Byte-wise validation: len()==6 alone is not enough — a
                // multi-byte char would be split by the [0..2] slices below.
                if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                Some(Color::Rgb(r, g, b))
            }
            serde_json::Value::Number(n) => n
                .as_u64()
                .and_then(|i| u8::try_from(i).ok())
                .map(Color::Indexed),
            _ => None,
        }
    }

    fn sgr_fg(self) -> String {
        match self {
            Color::Default => "39".to_string(),
            Color::Indexed(i) => format!("38;5;{i}"),
            Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        }
    }

    fn sgr_bg(self) -> String {
        match self {
            Color::Default => "49".to_string(),
            Color::Indexed(i) => format!("48;5;{i}"),
            Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        }
    }
}

/// Text style attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub inverse: bool,
    pub strikethrough: bool,
}

impl Style {
    pub fn new() -> Self {
        Style::default()
    }

    pub fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    pub fn underline(mut self) -> Self {
        self.underline = true;
        self
    }

    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }

    pub fn inverse(mut self) -> Self {
        self.inverse = true;
        self
    }

    /// True when this style changes nothing.
    pub fn is_plain(&self) -> bool {
        *self == Style::default()
    }

    /// Merge `over` on top of `self` (set fields win).
    pub fn merged_with(&self, over: &Style) -> Style {
        Style {
            fg: over.fg.or(self.fg),
            bg: over.bg.or(self.bg),
            bold: self.bold || over.bold,
            italic: self.italic || over.italic,
            underline: self.underline || over.underline,
            dim: self.dim || over.dim,
            inverse: self.inverse || over.inverse,
            strikethrough: self.strikethrough || over.strikethrough,
        }
    }

    /// ANSI SGR prefix for this style (empty when plain).
    pub fn ansi_prefix(&self) -> String {
        if self.is_plain() {
            return String::new();
        }
        let mut codes: Vec<String> = Vec::new();
        if self.bold {
            codes.push("1".into());
        }
        if self.dim {
            codes.push("2".into());
        }
        if self.italic {
            codes.push("3".into());
        }
        if self.underline {
            codes.push("4".into());
        }
        if self.inverse {
            codes.push("7".into());
        }
        if self.strikethrough {
            codes.push("9".into());
        }
        if let Some(fg) = self.fg {
            codes.push(fg.sgr_fg());
        }
        if let Some(bg) = self.bg {
            codes.push(bg.sgr_bg());
        }
        format!("\x1b[{}m", codes.join(";"))
    }

    /// ANSI reset suffix (only when the prefix did anything).
    pub fn ansi_suffix(&self) -> &'static str {
        if self.is_plain() { "" } else { "\x1b[0m" }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_indexed() {
        assert_eq!(
            Color::parse(&serde_json::json!("#ff8000")),
            Some(Color::Rgb(255, 128, 0))
        );
        assert_eq!(
            Color::parse(&serde_json::json!(196)),
            Some(Color::Indexed(196))
        );
        assert_eq!(Color::parse(&serde_json::json!("red")), None);
    }

    #[test]
    fn non_ascii_hex_does_not_panic() {
        // "#a€bc" is exactly 6 bytes but '€' spans 3 of them — slicing
        // hex[0..2] / hex[2..4] / hex[4..6] splits the char and panics.
        assert_eq!(Color::parse(&serde_json::json!("#a€bc")), None);
        assert_eq!(Color::parse(&serde_json::json!("#€€")), None);
    }

    #[test]
    fn out_of_range_indexed_is_rejected_not_wrapped() {
        assert_eq!(
            Color::parse(&serde_json::json!(255)),
            Some(Color::Indexed(255))
        );
        // 256 as u8 wraps to 0 — a theme typo must not silently turn black.
        assert_eq!(Color::parse(&serde_json::json!(256)), None);
        assert_eq!(Color::parse(&serde_json::json!(-1)), None);
    }

    #[test]
    fn sgr_output() {
        assert_eq!(Style::new().ansi_prefix(), "");
        assert_eq!(
            Style::new().bold().fg(Color::Indexed(1)).ansi_prefix(),
            "\x1b[1;38;5;1m"
        );
        assert_eq!(
            Style::new().bg(Color::Rgb(1, 2, 3)).ansi_prefix(),
            "\x1b[48;2;1;2;3m"
        );
    }
}
