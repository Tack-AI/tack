//! Spinner/loader component (port of `loader.ts`): animated frame + label.

use crate::component::Component;
use crate::line::{Line, Span};
use crate::style::{Color, Style};

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A spinner with a label. `tick` advances the frame (driven by a timer).
#[derive(Debug)]
pub struct Loader {
    label: String,
    frame: usize,
    pub spinner_style: Style,
    pub text_style: Style,
}

impl Loader {
    pub fn new(label: impl Into<String>) -> Self {
        Loader {
            label: label.into(),
            frame: 0,
            spinner_style: Style::new().fg(Color::Rgb(255, 140, 0)),
            text_style: Style::default(),
        }
    }

    pub fn set_label(&mut self, label: impl Into<String>) {
        self.label = label.into();
    }

    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % FRAMES.len();
    }
}

impl Component for Loader {
    fn render(&mut self, _width: u16) -> Vec<Line> {
        vec![Line::from_spans(vec![
            Span::styled(FRAMES[self.frame], self.spinner_style),
            Span::plain(" "),
            Span::styled(self.label.clone(), self.text_style),
        ])]
    }
}
