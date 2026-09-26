//! Static text component: wraps styled spans to the viewport width.

use crate::component::Component;
use crate::line::{Line, Span};
use crate::style::Style;

/// A block of styled text, word-wrapped at render time.
#[derive(Clone, Debug, Default)]
pub struct Text {
    spans: Vec<Span>,
    /// Left/right cell padding.
    pub padding_x: u16,
    cache: Option<(u16, Vec<Line>)>,
}

impl Text {
    pub fn new(text: impl Into<String>, style: Style) -> Self {
        Text {
            spans: vec![Span::styled(text.into(), style)],
            padding_x: 0,
            cache: None,
        }
    }

    pub fn from_spans(spans: Vec<Span>) -> Self {
        Text {
            spans,
            padding_x: 0,
            cache: None,
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>, style: Style) {
        self.spans = vec![Span::styled(text.into(), style)];
        self.invalidate();
    }

    pub fn set_spans(&mut self, spans: Vec<Span>) {
        self.spans = spans;
        self.invalidate();
    }
}

impl Component for Text {
    fn render(&mut self, width: u16) -> Vec<Line> {
        if let Some((cached_width, ref lines)) = self.cache
            && cached_width == width
        {
            return lines.clone();
        }
        let inner = width.saturating_sub(self.padding_x * 2) as usize;
        // Split on hard newlines first, then wrap each.
        let mut lines: Vec<Line> = Vec::new();
        let mut current = Line::new();
        for span in &self.spans {
            let mut rest = &*span.text;
            loop {
                match rest.find('\n') {
                    Some(pos) => {
                        current.push(Span::styled(&rest[..pos], span.style));
                        lines.push(std::mem::take(&mut current));
                        rest = &rest[pos + 1..];
                    }
                    None => {
                        current.push(Span::styled(rest, span.style));
                        break;
                    }
                }
            }
        }
        lines.push(current);
        let mut wrapped: Vec<Line> = lines.iter().flat_map(|l| l.wrap(inner.max(1))).collect();
        if self.padding_x > 0 {
            for line in &mut wrapped {
                line.spans
                    .insert(0, Span::plain(" ".repeat(self.padding_x as usize)));
            }
        }
        self.cache = Some((width, wrapped.clone()));
        wrapped
    }

    fn invalidate(&mut self) {
        self.cache = None;
    }
}
