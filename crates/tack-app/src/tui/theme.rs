//! Theme: color slots for the TUI (port of theme.ts slot names). Built-ins:
//! dark/light plus popular community palettes (Catppuccin, Tokyo Night,
//! Gruvbox, Nord, Dracula, One Dark, Solarized, Kanagawa, Rosé Pine);
//! user JSON themes load from the themes dirs (see `Theme::load`).

use std::path::{Path, PathBuf};

use tack_tui::components::markdown::MarkdownTheme;
use tack_tui::{Color, Style};

/// 0xRRGGBB hex → Color (keeps built-in palettes compact).
const fn rgb(v: u32) -> Color {
    Color::Rgb(
        ((v >> 16) & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        (v & 0xff) as u8,
    )
}

/// Palette spec for a built-in theme (0xRRGGBB values). Slots not listed
/// derive from these in `Theme::from_spec`, mirroring the dark built-in's
/// relationships (tool_title = text, tool_output = muted, thinking/quote =
/// italic muted, link_url = dim, list_bullet = accent, diff = success/error).
struct Spec {
    accent: u32,
    border: u32,
    border_accent: u32,
    success: u32,
    error: u32,
    warning: u32,
    muted: u32,
    dim: u32,
    text: u32,
    user_message_bg: u32,
    tool_pending_bg: u32,
    tool_success_bg: u32,
    tool_error_bg: u32,
    selected_bg: u32,
    md_heading: u32,
    md_code: u32,
    md_code_block: u32,
    md_link: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub accent: Style,
    pub border: Style,
    pub border_accent: Style,
    pub success: Style,
    pub error: Style,
    pub warning: Style,
    pub muted: Style,
    pub dim: Style,
    pub text: Style,
    pub thinking_text: Style,
    pub user_message_bg: Style,
    pub tool_pending_bg: Style,
    pub tool_success_bg: Style,
    pub tool_error_bg: Style,
    pub tool_title: Style,
    pub tool_output: Style,
    pub selected_bg: Style,
    pub diff_added: Style,
    pub diff_removed: Style,
    pub markdown: MarkdownTheme,
}

impl Theme {
    /// TS pi dark.json. Note: `dim`/`tool_output` are REAL colors, not the
    /// SGR dim attribute (which is nearly invisible on Warp/Windows
    /// Terminal); Indexed(8) palettes vary per terminal, so muted grays are
    /// fixed RGB values.
    pub fn dark() -> Self {
        Theme {
            accent: Style::new().fg(Color::Rgb(0x8a, 0xbe, 0xb7)),
            border: Style::new().fg(Color::Rgb(0x5f, 0x87, 0xff)),
            border_accent: Style::new().fg(Color::Rgb(0x00, 0xd7, 0xff)),
            success: Style::new().fg(Color::Rgb(0xb5, 0xbd, 0x68)),
            error: Style::new().fg(Color::Rgb(0xcc, 0x66, 0x66)),
            warning: Style::new().fg(Color::Rgb(0xff, 0xff, 0x00)),
            muted: Style::new().fg(Color::Rgb(0x80, 0x80, 0x80)),
            dim: Style::new().fg(Color::Rgb(0x66, 0x66, 0x66)),
            text: Style::new().fg(Color::Rgb(0xd4, 0xd4, 0xd4)),
            thinking_text: Style::new().italic().fg(Color::Rgb(0x80, 0x80, 0x80)),
            user_message_bg: Style::new().bg(Color::Rgb(0x34, 0x35, 0x41)),
            tool_pending_bg: Style::new().bg(Color::Rgb(0x28, 0x28, 0x32)),
            tool_success_bg: Style::new().bg(Color::Rgb(0x28, 0x32, 0x28)),
            tool_error_bg: Style::new().bg(Color::Rgb(0x3c, 0x28, 0x28)),
            tool_title: Style::new().fg(Color::Rgb(0xd4, 0xd4, 0xd4)),
            tool_output: Style::new().fg(Color::Rgb(0x80, 0x80, 0x80)),
            selected_bg: Style::new().bg(Color::Rgb(0x3a, 0x3a, 0x4a)),
            diff_added: Style::new().fg(Color::Rgb(0xb5, 0xbd, 0x68)),
            diff_removed: Style::new().fg(Color::Rgb(0xcc, 0x66, 0x66)),
            markdown: MarkdownTheme::dark(),
        }
    }

    /// TS pi light.json.
    pub fn light() -> Self {
        Theme {
            accent: Style::new().fg(Color::Rgb(0x5a, 0x80, 0x80)),
            border: Style::new().fg(Color::Rgb(0x54, 0x7d, 0xa7)),
            border_accent: Style::new().fg(Color::Rgb(0x5a, 0x80, 0x80)),
            success: Style::new().fg(Color::Rgb(0x58, 0x84, 0x58)),
            error: Style::new().fg(Color::Rgb(0xaa, 0x55, 0x55)),
            warning: Style::new().fg(Color::Rgb(0x9a, 0x73, 0x26)),
            muted: Style::new().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            dim: Style::new().fg(Color::Rgb(0x76, 0x76, 0x76)),
            text: Style::new().fg(Color::Rgb(0x1f, 0x23, 0x28)),
            thinking_text: Style::new().italic().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            user_message_bg: Style::new().bg(Color::Rgb(0xe8, 0xe8, 0xe8)),
            tool_pending_bg: Style::new().bg(Color::Rgb(0xe8, 0xe8, 0xf0)),
            tool_success_bg: Style::new().bg(Color::Rgb(0xe8, 0xf0, 0xe8)),
            tool_error_bg: Style::new().bg(Color::Rgb(0xf0, 0xe8, 0xe8)),
            tool_title: Style::new().fg(Color::Rgb(0x1f, 0x23, 0x28)),
            tool_output: Style::new().fg(Color::Rgb(0x6c, 0x6c, 0x6c)),
            selected_bg: Style::new().bg(Color::Rgb(0xd0, 0xd0, 0xe0)),
            diff_added: Style::new().fg(Color::Rgb(0x58, 0x84, 0x58)),
            diff_removed: Style::new().fg(Color::Rgb(0xaa, 0x55, 0x55)),
            markdown: MarkdownTheme::light(),
        }
    }

    /// Build a theme from a palette spec (see `Spec` for the derivations).
    fn from_spec(s: Spec) -> Self {
        Theme {
            accent: Style::new().fg(rgb(s.accent)),
            border: Style::new().fg(rgb(s.border)),
            border_accent: Style::new().fg(rgb(s.border_accent)),
            success: Style::new().fg(rgb(s.success)),
            error: Style::new().fg(rgb(s.error)),
            warning: Style::new().fg(rgb(s.warning)),
            muted: Style::new().fg(rgb(s.muted)),
            dim: Style::new().fg(rgb(s.dim)),
            text: Style::new().fg(rgb(s.text)),
            thinking_text: Style::new().italic().fg(rgb(s.muted)),
            user_message_bg: Style::new().bg(rgb(s.user_message_bg)),
            tool_pending_bg: Style::new().bg(rgb(s.tool_pending_bg)),
            tool_success_bg: Style::new().bg(rgb(s.tool_success_bg)),
            tool_error_bg: Style::new().bg(rgb(s.tool_error_bg)),
            tool_title: Style::new().fg(rgb(s.text)),
            tool_output: Style::new().fg(rgb(s.muted)),
            selected_bg: Style::new().bg(rgb(s.selected_bg)),
            diff_added: Style::new().fg(rgb(s.success)),
            diff_removed: Style::new().fg(rgb(s.error)),
            markdown: MarkdownTheme {
                heading: Style::new().bold().fg(rgb(s.md_heading)),
                code: Style::new().fg(rgb(s.md_code)),
                code_block: Style::new().fg(rgb(s.md_code_block)),
                code_block_border: Style::new().fg(rgb(s.muted)),
                quote: Style::new().italic().fg(rgb(s.muted)),
                quote_border: Style::new().fg(rgb(s.muted)),
                link: Style::new().underline().fg(rgb(s.md_link)),
                link_url: Style::new().fg(rgb(s.dim)),
                list_bullet: Style::new().fg(rgb(s.accent)),
                hr: Style::new().fg(rgb(s.muted)),
                code_block_indent: 0,
            },
        }
    }

    /// Catppuccin Mocha (dark) — official catppuccin.com palette.
    pub fn catppuccin_mocha() -> Self {
        Self::from_spec(Spec {
            accent: 0x94e2d5,          // teal
            border: 0x89b4fa,          // blue
            border_accent: 0x89dceb,   // sky
            success: 0xa6e3a1,         // green
            error: 0xf38ba8,           // red
            warning: 0xf9e2af,         // yellow
            muted: 0x7f849c,           // overlay1
            dim: 0x6c7086,             // overlay0
            text: 0xcdd6f4,            // text
            user_message_bg: 0x313244, // surface0
            tool_pending_bg: 0x28283a, // base/surface0 blend
            tool_success_bg: 0x27362b, // dark green tint
            tool_error_bg: 0x3b2930,   // dark red tint
            selected_bg: 0x45475a,     // surface1
            md_heading: 0xf9e2af,      // yellow
            md_code: 0x94e2d5,         // teal
            md_code_block: 0xa6e3a1,   // green
            md_link: 0x89b4fa,         // blue
        })
    }

    /// Catppuccin Latte (light) — official catppuccin.com palette.
    pub fn catppuccin_latte() -> Self {
        Self::from_spec(Spec {
            accent: 0x179299,          // teal
            border: 0x1e66f5,          // blue
            border_accent: 0x04a5e5,   // sky
            success: 0x40a02b,         // green
            error: 0xd20f39,           // red
            warning: 0xdf8e1d,         // yellow
            muted: 0x8c8fa1,           // overlay1
            dim: 0x9ca0b0,             // overlay0
            text: 0x4c4f69,            // text
            user_message_bg: 0xe6e9ef, // mantle
            tool_pending_bg: 0xdfe4ee, // light blue-gray tint
            tool_success_bg: 0xddeedd, // light green tint
            tool_error_bg: 0xf3dee2,   // light red tint
            selected_bg: 0xccd0da,     // surface0
            md_heading: 0xdf8e1d,      // yellow
            md_code: 0x179299,         // teal
            md_code_block: 0x40a02b,   // green
            md_link: 0x1e66f5,         // blue
        })
    }

    /// Tokyo Night (dark) — tokyo-night palette (Night variant).
    pub fn tokyo_night() -> Self {
        Self::from_spec(Spec {
            accent: 0x7dcfff,          // cyan
            border: 0x7aa2f7,          // blue
            border_accent: 0x2ac3de,   // blue1
            success: 0x9ece6a,         // green
            error: 0xf7768e,           // red
            warning: 0xe0af68,         // yellow
            muted: 0x737aa2,           // dark5
            dim: 0x565f89,             // comment
            text: 0xc0caf5,            // fg
            user_message_bg: 0x24283b, // storm bg
            tool_pending_bg: 0x1f2335, // bg dark blend
            tool_success_bg: 0x22332b, // dark green tint
            tool_error_bg: 0x342634,   // dark red tint
            selected_bg: 0x2e3c64,     // visual selection
            md_heading: 0xe0af68,      // yellow
            md_code: 0x73daca,         // green1
            md_code_block: 0x9ece6a,   // green
            md_link: 0x7aa2f7,         // blue
        })
    }

    /// Gruvbox Dark — morhetz/gruvbox palette.
    pub fn gruvbox_dark() -> Self {
        Self::from_spec(Spec {
            accent: 0x8ec07c,          // aqua
            border: 0x83a598,          // blue
            border_accent: 0x8ec07c,   // aqua
            success: 0xb8bb26,         // green
            error: 0xfb4934,           // bright red
            warning: 0xfabd2f,         // bright yellow
            muted: 0xa89984,           // fg4
            dim: 0x928374,             // gray
            text: 0xebdbb2,            // fg1
            user_message_bg: 0x3c3836, // bg1
            tool_pending_bg: 0x32302f, // bg0/bg1 blend
            tool_success_bg: 0x363b24, // dark green tint
            tool_error_bg: 0x472e2b,   // dark red tint
            selected_bg: 0x504945,     // bg2
            md_heading: 0xfabd2f,      // yellow
            md_code: 0x8ec07c,         // aqua
            md_code_block: 0xb8bb26,   // green
            md_link: 0x83a598,         // blue
        })
    }

    /// Gruvbox Light — morhetz/gruvbox palette.
    pub fn gruvbox_light() -> Self {
        Self::from_spec(Spec {
            accent: 0x689d6a,          // aqua
            border: 0x458588,          // blue
            border_accent: 0x689d6a,   // aqua
            success: 0x98971a,         // green
            error: 0xcc241d,           // red
            warning: 0xb57614,         // faded yellow
            muted: 0x7c6f64,           // fg4
            dim: 0x928374,             // gray
            text: 0x3c3836,            // fg1
            user_message_bg: 0xebdbb2, // bg1
            tool_pending_bg: 0xefe5c5, // light yellow tint
            tool_success_bg: 0xe6e8c0, // light green tint
            tool_error_bg: 0xf0d5cb,   // light red tint
            selected_bg: 0xd5c4a1,     // bg2
            md_heading: 0xb57614,      // faded yellow
            md_code: 0x427b58,         // faded aqua
            md_code_block: 0x6c782e,   // faded green
            md_link: 0x076678,         // faded blue
        })
    }

    /// Nord (dark) — nordtheme.com palette.
    pub fn nord() -> Self {
        Self::from_spec(Spec {
            accent: 0x8fbcbb,          // nord7 (frost)
            border: 0x81a1c1,          // nord9
            border_accent: 0x88c0d0,   // nord8
            success: 0xa3be8c,         // nord14 (aurora green)
            error: 0xbf616a,           // nord11 (aurora red)
            warning: 0xebcb8b,         // nord13 (aurora yellow)
            muted: 0x7f8ca3,           // brightened nord3
            dim: 0x616e88,             // nord comment gray
            text: 0xd8dee9,            // nord4 (snow storm)
            user_message_bg: 0x3b4252, // nord1
            tool_pending_bg: 0x363d4a, // nord0/nord1 blend
            tool_success_bg: 0x3a4439, // dark green tint
            tool_error_bg: 0x473a3e,   // dark red tint
            selected_bg: 0x434c5e,     // nord2
            md_heading: 0xebcb8b,      // nord13
            md_code: 0x8fbcbb,         // nord7
            md_code_block: 0xa3be8c,   // nord14
            md_link: 0x81a1c1,         // nord9
        })
    }

    /// Dracula (dark) — draculatheme.com palette.
    pub fn dracula() -> Self {
        Self::from_spec(Spec {
            accent: 0xff79c6,          // pink
            border: 0xbd93f9,          // purple
            border_accent: 0x8be9fd,   // cyan
            success: 0x50fa7b,         // green
            error: 0xff5555,           // red
            warning: 0xf1fa8c,         // yellow
            muted: 0x6272a4,           // comment
            dim: 0x4d5570,             // darker comment (tuned)
            text: 0xf8f8f2,            // foreground
            user_message_bg: 0x3a3c4e, // bg/current-line blend
            tool_pending_bg: 0x2f3140, // darker current line
            tool_success_bg: 0x2f4237, // dark green tint
            tool_error_bg: 0x473239,   // dark red tint
            selected_bg: 0x44475a,     // current line / selection
            md_heading: 0xff79c6,      // pink
            md_code: 0x8be9fd,         // cyan
            md_code_block: 0x50fa7b,   // green
            md_link: 0xbd93f9,         // purple
        })
    }

    /// One Dark (Atom) — atom/one-dark-syntax palette.
    pub fn one_dark() -> Self {
        Self::from_spec(Spec {
            accent: 0x56b6c2,          // cyan
            border: 0x61afef,          // blue
            border_accent: 0x56b6c2,   // cyan
            success: 0x98c379,         // green
            error: 0xe06c75,           // red
            warning: 0xe5c07b,         // yellow
            muted: 0x828997,           // brightened comment
            dim: 0x5c6370,             // comment gray
            text: 0xabb2bf,            // fg
            user_message_bg: 0x31353f, // gutter bg
            tool_pending_bg: 0x2c313a, // darker gutter
            tool_success_bg: 0x2d3a2e, // dark green tint
            tool_error_bg: 0x3e3034,   // dark red tint
            selected_bg: 0x3e4451,     // selection
            md_heading: 0xe5c07b,      // yellow
            md_code: 0x56b6c2,         // cyan
            md_code_block: 0x98c379,   // green
            md_link: 0x61afef,         // blue
        })
    }

    /// Solarized Dark — ethanschoonover.com canonical palette.
    pub fn solarized_dark() -> Self {
        Self::from_spec(Spec {
            accent: 0x2aa198,          // cyan
            border: 0x268bd2,          // blue
            border_accent: 0x2aa198,   // cyan
            success: 0x859900,         // green
            error: 0xdc322f,           // red
            warning: 0xb58900,         // yellow
            muted: 0x839496,           // base0
            dim: 0x586e75,             // base01
            text: 0x93a1a1,            // base1
            user_message_bg: 0x073642, // base02
            tool_pending_bg: 0x05303a, // base02/base03 blend
            tool_success_bg: 0x0e3626, // dark green tint
            tool_error_bg: 0x3f2826,   // dark red tint
            selected_bg: 0x0f4754,     // lightened base02
            md_heading: 0xb58900,      // yellow
            md_code: 0x2aa198,         // cyan
            md_code_block: 0x859900,   // green
            md_link: 0x268bd2,         // blue
        })
    }

    /// Solarized Light — ethanschoonover.com canonical palette.
    pub fn solarized_light() -> Self {
        Self::from_spec(Spec {
            accent: 0x2aa198,          // cyan
            border: 0x268bd2,          // blue
            border_accent: 0x2aa198,   // cyan
            success: 0x859900,         // green
            error: 0xdc322f,           // red
            warning: 0xb58900,         // yellow
            muted: 0x93a1a1,           // base1
            dim: 0xafb9b4,             // lightened base1 (tuned)
            text: 0x657b83,            // base00
            user_message_bg: 0xeee8d5, // base2
            tool_pending_bg: 0xf0ebda, // light yellow tint
            tool_success_bg: 0xe7ebcf, // light green tint
            tool_error_bg: 0xf3ddd4,   // light red tint
            selected_bg: 0xe3dcc6,     // darkened base2
            md_heading: 0xb58900,      // yellow
            md_code: 0x2aa198,         // cyan
            md_code_block: 0x859900,   // green
            md_link: 0x268bd2,         // blue
        })
    }

    /// Kanagawa (dark) — rebelot/kanagawa.nvim wave palette.
    pub fn kanagawa() -> Self {
        Self::from_spec(Spec {
            accent: 0x7fb4ca,          // springBlue
            border: 0x7e9cd8,          // crystalBlue
            border_accent: 0x6a9589,   // waveAqua
            success: 0x98bb6c,         // springGreen
            error: 0xe46876,           // waveRed
            warning: 0xff9e3b,         // roninYellow
            muted: 0x727169,           // fujiGray
            dim: 0x54546d,             // sumiInk4
            text: 0xdcd7ba,            // fujiWhite
            user_message_bg: 0x2a2a37, // sumiInk2
            tool_pending_bg: 0x25252f, // sumiInk1/2 blend
            tool_success_bg: 0x2b3328, // dark green tint
            tool_error_bg: 0x3d2b2e,   // dark red tint
            selected_bg: 0x363646,     // sumiInk3
            md_heading: 0xe6c384,      // carpYellow
            md_code: 0x7fb4ca,         // springBlue
            md_code_block: 0x98bb6c,   // springGreen
            md_link: 0x7e9cd8,         // crystalBlue
        })
    }

    /// Monokai (dark) — classic Sublime Text palette.
    pub fn monokai() -> Self {
        Self::from_spec(Spec {
            accent: 0x66d9ef,          // cyan
            border: 0xae81ff,          // purple
            border_accent: 0x66d9ef,   // cyan
            success: 0xa6e22e,         // green
            error: 0xf92672,           // pink/red
            warning: 0xe6db74,         // yellow
            muted: 0x75715e,           // comment
            dim: 0x5e5a4b,             // darker comment (tuned)
            text: 0xf8f8f2,            // foreground
            user_message_bg: 0x3e3d32, // line highlight
            tool_pending_bg: 0x2f3027, // bg/highlight blend
            tool_success_bg: 0x2e3a23, // dark green tint
            tool_error_bg: 0x3e2530,   // dark red tint
            selected_bg: 0x49483e,     // selection
            md_heading: 0xe6db74,      // yellow
            md_code: 0x66d9ef,         // cyan
            md_code_block: 0xa6e22e,   // green
            md_link: 0xae81ff,         // purple
        })
    }

    /// Rosé Pine (dark) — rosepinetheme.com main palette.
    pub fn rose_pine() -> Self {
        Self::from_spec(Spec {
            accent: 0x9ccfd8,          // foam
            border: 0xc4a7e7,          // iris
            border_accent: 0x9ccfd8,   // foam
            success: 0x31748f,         // pine
            error: 0xeb6f92,           // love
            warning: 0xf6c177,         // gold
            muted: 0x908caa,           // subtle
            dim: 0x6e6a86,             // muted
            text: 0xe0def4,            // text
            user_message_bg: 0x26233a, // overlay
            tool_pending_bg: 0x21202e, // highlight low
            tool_success_bg: 0x23333a, // dark foam tint
            tool_error_bg: 0x3a2530,   // dark love tint
            selected_bg: 0x403d52,     // highlight med
            md_heading: 0xf6c177,      // gold
            md_code: 0xebbcba,         // rose
            md_code_block: 0x9ccfd8,   // foam
            md_link: 0xc4a7e7,         // iris
        })
    }

    /// Rosé Pine Dawn (light) — rosepinetheme.com dawn palette.
    pub fn rose_pine_dawn() -> Self {
        Self::from_spec(Spec {
            accent: 0x56949f,          // foam
            border: 0x907aa9,          // iris
            border_accent: 0x56949f,   // foam
            success: 0x286983,         // pine
            error: 0xb4637a,           // love
            warning: 0xea9d34,         // gold
            muted: 0x797593,           // subtle
            dim: 0x9893a5,             // muted
            text: 0x575279,            // text
            user_message_bg: 0xf2e9e1, // overlay
            tool_pending_bg: 0xf6efe8, // light gray tint
            tool_success_bg: 0xe3ecec, // light foam tint
            tool_error_bg: 0xf4e4e4,   // light love tint
            selected_bg: 0xdfdad9,     // highlight med
            md_heading: 0xea9d34,      // gold
            md_code: 0xd7827e,         // rose
            md_code_block: 0x56949f,   // foam
            md_link: 0x286983,         // pine
        })
    }

    /// Every built-in theme name (dark/light first, then community palettes).
    pub const BUILTIN_NAMES: &'static [&'static str] = &[
        "dark",
        "light",
        "catppuccin-mocha",
        "catppuccin-latte",
        "tokyo-night",
        "gruvbox-dark",
        "gruvbox-light",
        "nord",
        "dracula",
        "one-dark",
        "solarized-dark",
        "solarized-light",
        "kanagawa",
        "monokai",
        "rose-pine",
        "rose-pine-dawn",
    ];

    /// Is `name` a built-in theme?
    pub fn is_builtin(name: &str) -> bool {
        Self::BUILTIN_NAMES.contains(&name)
    }

    /// Built-in theme by name (None for user/custom names).
    pub fn builtin(name: &str) -> Option<Theme> {
        Some(match name {
            "dark" => Theme::dark(),
            "light" => Theme::light(),
            "catppuccin-mocha" => Theme::catppuccin_mocha(),
            "catppuccin-latte" => Theme::catppuccin_latte(),
            "tokyo-night" => Theme::tokyo_night(),
            "gruvbox-dark" => Theme::gruvbox_dark(),
            "gruvbox-light" => Theme::gruvbox_light(),
            "nord" => Theme::nord(),
            "dracula" => Theme::dracula(),
            "one-dark" => Theme::one_dark(),
            "solarized-dark" => Theme::solarized_dark(),
            "solarized-light" => Theme::solarized_light(),
            "kanagawa" => Theme::kanagawa(),
            "monokai" => Theme::monokai(),
            "rose-pine" => Theme::rose_pine(),
            "rose-pine-dawn" => Theme::rose_pine_dawn(),
            _ => return None,
        })
    }

    /// Resolve from settings: built-in name (`dark`/`light`/community
    /// palettes), a custom theme name from the themes dirs, or a
    /// `"light/dark"` pair auto-selected by the terminal background (TS
    /// parseAutoThemeSetting + OSC 11 detection).
    pub fn resolve(name: Option<&str>, agent_dir: &Path, cwd: &Path) -> Self {
        if let Some(setting) = name
            && let Some((light, dark)) = parse_auto_pair(setting)
        {
            let chosen = if terminal_is_dark() { dark } else { light };
            return Theme::resolve(Some(chosen), agent_dir, cwd);
        }
        match name {
            Some(n) => Theme::builtin(n)
                .or_else(|| Theme::load(n, agent_dir, cwd))
                .unwrap_or_else(Theme::dark),
            None => Theme::dark(),
        }
    }

    /// Built-ins only (`--no-themes`); unknown names fall back to dark.
    pub fn from_name(name: Option<&str>) -> Self {
        name.and_then(Theme::builtin).unwrap_or_else(Theme::dark)
    }

    /// Load a user theme by name from the themes dirs
    /// (`<agentDir>/themes/<name>.json`, `<cwd>/.pi/themes/<name>.json`).
    /// Slot names match TS pi's theme schema (accent, border, success, …);
    /// unknown slots are ignored. Falls back to the built-in on parse errors.
    pub fn load(name: &str, agent_dir: &Path, cwd: &Path) -> Option<Theme> {
        // Project dir first (wins on collision), but trust-gated.
        let mut candidates = Vec::new();
        if crate::project_trust::is_trusted(cwd, agent_dir) {
            candidates.push(cwd.join(".pi").join("themes").join(format!("{name}.json")));
        }
        candidates.push(agent_dir.join("themes").join(format!("{name}.json")));
        // Extra theme files/dirs from settings `themes` (TS getThemePaths).
        for extra in crate::settings::Settings::extra_paths(agent_dir, "themes") {
            let path = PathBuf::from(extra);
            if path.is_dir() {
                candidates.push(path.join(format!("{name}.json")));
            } else if path.file_stem().is_some_and(|s| s == name) {
                candidates.push(path);
            }
        }
        let content = candidates
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok())?;
        let value: serde_json::Value = serde_json::from_str(&content).ok()?;
        Some(Theme::from_json(&value))
    }

    /// All theme names available (built-ins + themes dirs).
    pub fn available(agent_dir: &Path, cwd: &Path) -> Vec<String> {
        let mut names: Vec<String> = Self::BUILTIN_NAMES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut dirs = vec![agent_dir.join("themes")];
        if crate::project_trust::is_trusted(cwd, agent_dir) {
            dirs.push(cwd.join(".pi").join("themes"));
        }
        for extra in crate::settings::Settings::extra_paths(agent_dir, "themes") {
            let path = PathBuf::from(&extra);
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "json")
                && let Some(stem) = path.file_stem()
            {
                names.push(stem.to_string_lossy().to_string());
            }
        }
        for dir in dirs {
            if let Ok(read) = std::fs::read_dir(dir) {
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json")
                        && let Some(stem) = path.file_stem()
                    {
                        names.push(stem.to_string_lossy().to_string());
                    }
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }

    /// Apply a theme JSON over the dark built-in. Values: "#rrggbb" or a
    /// 256-palette number.
    pub fn from_json(value: &serde_json::Value) -> Theme {
        let mut theme = Theme::dark();
        let colors = value.get("colors").unwrap_or(value);
        let get = |key: &str| colors.get(key).and_then(Color::parse);
        macro_rules! slot {
            ($field:ident, $key:literal) => {
                if let Some(c) = get($key) {
                    theme.$field.fg = Some(c);
                }
            };
            (bg $field:ident, $key:literal) => {
                if let Some(c) = get($key) {
                    theme.$field.bg = Some(c);
                }
            };
        }
        slot!(accent, "accent");
        slot!(border, "border");
        slot!(border_accent, "borderAccent");
        slot!(success, "success");
        slot!(error, "error");
        slot!(warning, "warning");
        slot!(muted, "muted");
        slot!(dim, "dim");
        slot!(text, "text");
        slot!(thinking_text, "thinkingText");
        slot!(bg user_message_bg, "userMessageBg");
        slot!(bg tool_pending_bg, "toolPendingBg");
        slot!(bg tool_success_bg, "toolSuccessBg");
        slot!(bg tool_error_bg, "toolErrorBg");
        slot!(tool_title, "toolTitle");
        slot!(tool_output, "toolOutput");
        slot!(bg selected_bg, "selectedBg");
        slot!(diff_added, "toolDiffAdded");
        slot!(diff_removed, "toolDiffRemoved");
        // Markdown slots.
        if let Some(c) = get("mdHeading") {
            theme.markdown.heading.fg = Some(c);
        }
        if let Some(c) = get("mdCode") {
            theme.markdown.code.fg = Some(c);
        }
        if let Some(c) = get("mdCodeBlock") {
            theme.markdown.code_block.fg = Some(c);
        }
        if let Some(c) = get("mdCodeBlockBorder") {
            theme.markdown.code_block_border.fg = Some(c);
        }
        if let Some(c) = get("mdQuote") {
            theme.markdown.quote.fg = Some(c);
        }
        if let Some(c) = get("mdQuoteBorder") {
            theme.markdown.quote_border.fg = Some(c);
        }
        if let Some(c) = get("mdLink") {
            theme.markdown.link.fg = Some(c);
        }
        if let Some(c) = get("mdLinkUrl") {
            theme.markdown.link_url.fg = Some(c);
        }
        if let Some(c) = get("mdListBullet") {
            theme.markdown.list_bullet.fg = Some(c);
        }
        if let Some(c) = get("mdHr") {
            theme.markdown.hr.fg = Some(c);
        }
        theme
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

/// TS parseAutoThemeSetting: exactly one "/" splits "lightTheme/darkTheme".
fn parse_auto_pair(setting: &str) -> Option<(&str, &str)> {
    let slash = setting.find('/')?;
    if setting[slash + 1..].contains('/') {
        return None;
    }
    let light = setting[..slash].trim();
    let dark = setting[slash + 1..].trim();
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light, dark))
}

/// Is the terminal background dark? OSC 11 query first, COLORFGBG env
/// fallback, dark otherwise (TS detectTerminalBackgroundTheme).
///
/// Detected ONCE per process: the query spawns a (detached) stdin reader
/// thread and waits for the terminal's reply, so it is designed as a
/// one-shot startup query — re-running it per render (e.g. from the mermaid
/// pipeline) leaks a stdin-stealing thread and stalls every frame.
pub(crate) fn terminal_is_dark() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Some((r, g, b)) = tack_tui::terminal::query_background_color() {
            // Perceived brightness (Rec. 601) above the midpoint = light theme.
            let brightness =
                (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)) / 255.0;
            return brightness <= 0.5;
        }
        if let Ok(value) = std::env::var("COLORFGBG") {
            // "fg;bg" (may have extra segments; bg is last).
            if let Some(bg) = value
                .rsplit(';')
                .next()
                .and_then(|s| s.trim().parse::<u8>().ok())
            {
                return !matches!(bg, 7 | 9..=15);
            }
        }
        true
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn auto_pair_parsing() {
        assert_eq!(parse_auto_pair("light/dark"), Some(("light", "dark")));
        assert_eq!(
            parse_auto_pair(" solarized-light / my-dark "),
            Some(("solarized-light", "my-dark"))
        );
        assert_eq!(parse_auto_pair("dark"), None);
        assert_eq!(parse_auto_pair("a/b/c"), None);
        assert_eq!(parse_auto_pair("/dark"), None);
    }

    #[test]
    fn all_builtins_resolve() {
        for name in Theme::BUILTIN_NAMES {
            let theme = Theme::builtin(name).unwrap_or_else(|| panic!("missing {name}"));
            // Sanity: core slots populated.
            assert!(theme.text.fg.is_some(), "{name} text");
            assert!(theme.accent.fg.is_some(), "{name} accent");
            assert!(theme.user_message_bg.bg.is_some(), "{name} user bg");
            assert!(theme.markdown.heading.fg.is_some(), "{name} md heading");
            assert!(Theme::is_builtin(name), "{name}");
            assert_eq!(
                Theme::from_name(Some(name)).text.fg,
                theme.text.fg,
                "from_name({name})"
            );
        }
    }

    #[test]
    fn builtins_listed_in_available() {
        let tmp = std::env::temp_dir().join("tack-theme-test-nonexistent");
        let names = Theme::available(&tmp, &tmp);
        for name in Theme::BUILTIN_NAMES {
            assert!(names.iter().any(|n| n == name), "available missing {name}");
        }
        // Sorted + deduped.
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn unknown_builtin_name_falls_back_to_dark() {
        assert!(Theme::builtin("no-such-theme").is_none());
        let dark = Theme::dark();
        assert_eq!(
            Theme::from_name(Some("no-such-theme")).text.fg,
            dark.text.fg
        );
        assert_eq!(Theme::from_name(None).text.fg, dark.text.fg);
    }
}
