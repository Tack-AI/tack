//! Vertical/horizontal stacks (port of `v-stack.ts` / `h-stack.ts`).

use crate::component::Component;
use crate::line::{Line, Span};

/// Children rendered top-to-bottom.
#[derive(Debug, Default)]
pub struct VStack {
    pub children: Vec<Box<dyn Component>>,
    /// Blank lines between children.
    pub gap: u16,
}

impl VStack {
    pub fn new() -> Self {
        VStack::default()
    }

    pub fn push(&mut self, component: impl Component + 'static) {
        self.children.push(Box::new(component));
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }
}

impl Component for VStack {
    fn render(&mut self, width: u16) -> Vec<Line> {
        let mut lines = Vec::new();
        for (i, child) in self.children.iter_mut().enumerate() {
            if i > 0 {
                for _ in 0..self.gap {
                    lines.push(Line::new());
                }
            }
            lines.extend(child.render(width));
        }
        lines
    }

    fn handle_input(&mut self, event: &crate::input::InputEvent) -> bool {
        // Last child (bottom-most) gets input first.
        self.children
            .iter_mut()
            .rev()
            .any(|c| c.handle_input(event))
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

/// Children rendered left-to-right, each wrapping within its share.
#[derive(Debug, Default)]
pub struct HStack {
    pub children: Vec<Box<dyn Component>>,
    pub gap: u16,
}

impl HStack {
    pub fn new() -> Self {
        HStack::default()
    }

    pub fn push(&mut self, component: impl Component + 'static) {
        self.children.push(Box::new(component));
    }
}

impl Component for HStack {
    fn render(&mut self, width: u16) -> Vec<Line> {
        if self.children.is_empty() {
            return Vec::new();
        }
        let gaps = self.gap as usize * (self.children.len().saturating_sub(1));
        let share = (width as usize).saturating_sub(gaps) / self.children.len();
        let rendered: Vec<Vec<Line>> = self
            .children
            .iter_mut()
            .map(|c| c.render(share.max(1) as u16))
            .collect();
        let rows = rendered.iter().map(Vec::len).max().unwrap_or(0);
        let mut lines = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = Line::new();
            for (i, child_lines) in rendered.iter().enumerate() {
                if i > 0 {
                    line.push(Span::plain(" ".repeat(self.gap as usize)));
                }
                match child_lines.get(row) {
                    Some(child_line) => {
                        for span in &child_line.spans {
                            line.push(span.clone());
                        }
                    }
                    None => line.push(Span::plain(" ".repeat(share))),
                }
            }
            lines.push(line);
        }
        lines
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}
