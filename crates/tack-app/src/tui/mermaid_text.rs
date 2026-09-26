//! Text-mode mermaid renderer: converts the positioned SVG from
//! mermaid-rs-renderer into a Unicode text diagram. Edges are rasterized
//! two ways: orthogonal routes (straight / 90° bends) as box-drawing
//! characters with proper corners, everything else (Bézier S-curves,
//! diagonals) on a Braille sub-cell layer (2×4 dots per cell) so curves
//! stay smooth instead of degenerating into jagged ╲╱ staircases. Node
//! frames, arrowheads and labels are hard characters drawn over the
//! Braille. Used when the terminal has no graphics protocol (Warp/Windows
//! Terminal/Termux), where half-block bitmaps are unreadable.

/// Parsed SVG geometry (mermaid-rs-renderer output is a single-line, regular
/// document — a hand-rolled scanner is sufficient and dependency-free).
#[derive(Debug)]
enum Element {
    NodeRect { x: f32, y: f32, w: f32, h: f32 },
    NodePoly { points: Vec<(f32, f32)> },
    NodeEllipse { cx: f32, cy: f32, rx: f32, ry: f32 },
    Edge { points: Vec<(f32, f32)> },
    Arrow { x: f32, y: f32 },
    Label { x: f32, y: f32, text: String },
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(pos) = tag[from..].find(&key) {
        let start = from + pos;
        // Attribute names start after whitespace (otherwise `d="` would match
        // inside `data-edge-id="` etc.).
        if tag.as_bytes()[start.saturating_sub(1)] == b' ' {
            let vstart = start + key.len();
            let vend = tag[vstart..].find('"')? + vstart;
            return Some(tag[vstart..vend].to_string());
        }
        from = start + 1;
    }
    None
}

fn attr_f32(tag: &str, name: &str) -> Option<f32> {
    attr(tag, name)?.parse().ok()
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Parse an SVG path `d` (M/L/H/V/C/S/Q/T/Z, comma- or space-separated,
/// relative forms included) into a sampled polyline.
fn sample_path(d: &str) -> Vec<(f32, f32)> {
    let mut pts: Vec<(f32, f32)> = Vec::new();
    let mut chars = d.chars().peekable();
    let read_num = |chars: &mut std::iter::Peekable<std::str::Chars>| -> Option<f32> {
        while matches!(chars.peek(), Some(',') | Some(' ')) {
            chars.next();
        }
        let mut buf = String::new();
        while let Some(&c) = chars.peek() {
            // Sign only at the start or in an exponent ("10-20" is two
            // numbers; "1e-3" is one); 'e' only once and after digits.
            let ok = c.is_ascii_digit()
                || c == '.'
                || ((c == '-' || c == '+')
                    && (buf.is_empty() || buf.ends_with('e') || buf.ends_with('E')))
                || ((c == 'e' || c == 'E')
                    && buf
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '+')
                    && !buf.contains(['e', 'E']));
            if ok {
                buf.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if buf.is_empty() {
            None
        } else {
            buf.parse().ok()
        }
    };
    let mut cur = (0.0f32, 0.0f32);
    let mut prev_ctrl: Option<(f32, f32)> = None; // last cubic/quadratic control point
    let mut cmd = ' ';
    macro_rules! num {
        () => {
            match read_num(&mut chars) {
                Some(v) => v,
                None => break,
            }
        };
    }
    loop {
        // A command letter, or implicit repeat of the previous command.
        while let Some(&c) = chars.peek() {
            if c == ' ' || c == ',' {
                chars.next();
            } else {
                break;
            }
        }
        match chars.peek().copied() {
            Some(c) if c.is_ascii_alphabetic() => {
                cmd = c;
                chars.next();
            }
            Some(_) => {}
            None => break,
        }
        let rel = cmd.is_ascii_lowercase();
        match cmd.to_ascii_uppercase() {
            'M' | 'L' => {
                let (mut x, mut y) = (num!(), num!());
                if rel {
                    x += cur.0;
                    y += cur.1;
                }
                cur = (x, y);
                pts.push(cur);
                prev_ctrl = None;
                if cmd == 'M' {
                    cmd = 'L'; // implicit lineto after moveto
                } else if cmd == 'm' {
                    cmd = 'l';
                }
            }
            'H' => {
                let mut x = num!();
                if rel {
                    x += cur.0;
                }
                cur.0 = x;
                pts.push(cur);
                prev_ctrl = None;
            }
            'V' => {
                let mut y = num!();
                if rel {
                    y += cur.1;
                }
                cur.1 = y;
                pts.push(cur);
                prev_ctrl = None;
            }
            'Q' | 'T' => {
                let (cx, cy) = if cmd.eq_ignore_ascii_case(&'q') {
                    let (mut cx, mut cy) = (num!(), num!());
                    if rel {
                        cx += cur.0;
                        cy += cur.1;
                    }
                    (cx, cy)
                } else {
                    // T: control point is the reflection of the previous one
                    // (already absolute).
                    prev_ctrl
                        .map(|(px, py)| (2.0 * cur.0 - px, 2.0 * cur.1 - py))
                        .unwrap_or(cur)
                };
                let (mut x, mut y) = (num!(), num!());
                if rel {
                    x += cur.0;
                    y += cur.1;
                }
                for i in 1..=12 {
                    let t = i as f32 / 12.0;
                    let mt = 1.0 - t;
                    pts.push((
                        mt * mt * cur.0 + 2.0 * mt * t * cx + t * t * x,
                        mt * mt * cur.1 + 2.0 * mt * t * cy + t * t * y,
                    ));
                }
                cur = (x, y);
                prev_ctrl = Some((cx, cy));
            }
            'C' | 'S' => {
                let (c1x, c1y) = if cmd.eq_ignore_ascii_case(&'c') {
                    let (mut c1x, mut c1y) = (num!(), num!());
                    if rel {
                        c1x += cur.0;
                        c1y += cur.1;
                    }
                    (c1x, c1y)
                } else {
                    // S: first control point is the reflection of the
                    // previous cubic's second one (already absolute).
                    prev_ctrl
                        .map(|(px, py)| (2.0 * cur.0 - px, 2.0 * cur.1 - py))
                        .unwrap_or(cur)
                };
                let (mut c2x, mut c2y) = (num!(), num!());
                let (mut x, mut y) = (num!(), num!());
                if rel {
                    c2x += cur.0;
                    c2y += cur.1;
                    x += cur.0;
                    y += cur.1;
                }
                for i in 1..=12 {
                    let t = i as f32 / 12.0;
                    let mt = 1.0 - t;
                    pts.push((
                        mt * mt * mt * cur.0
                            + 3.0 * mt * mt * t * c1x
                            + 3.0 * mt * t * t * c2x
                            + t * t * t * x,
                        mt * mt * mt * cur.1
                            + 3.0 * mt * mt * t * c1y
                            + 3.0 * mt * t * t * c2y
                            + t * t * t * y,
                    ));
                }
                cur = (x, y);
                prev_ctrl = Some((c2x, c2y));
            }
            'Z' => {
                prev_ctrl = None;
            }
            _ => {
                chars.next();
            }
        }
    }
    pts
}

/// Parse the mermaid SVG into positioned elements. Returns None when the
/// document looks unfamiliar (better to fall back to the bitmap).
fn parse_svg(svg: &str) -> Option<Vec<Element>> {
    let mut elements = Vec::new();
    let mut rest = svg;
    let mut in_defs = false;
    let mut svg_w = 0.0f32;
    let mut svg_h = 0.0f32;
    // Current <g> transform (arrowhead markers are positioned via it).
    let mut transform: Option<(f32, f32, f32)> = None;
    while let Some(open) = rest.find('<') {
        rest = &rest[open..];
        let end = rest.find('>')? + 1;
        let tag = &rest[..end];
        if tag.starts_with("<svg") {
            svg_w = attr_f32(tag, "width").unwrap_or(0.0);
            svg_h = attr_f32(tag, "height").unwrap_or(0.0);
        } else if tag.starts_with("<defs") {
            in_defs = true;
        } else if tag.starts_with("</defs") {
            in_defs = false;
        } else if tag.starts_with("<g") && !in_defs {
            if let Some(t) = attr(tag, "transform") {
                // translate(x y) rotate(deg): collect the numbers in order.
                let nums: Vec<f32> = t
                    .split(['(', ')', ' ', ','])
                    .filter_map(|s| s.parse().ok())
                    .collect();
                transform = match nums.as_slice() {
                    [x, y, deg, ..] => Some((*x, *y, *deg)),
                    [x, y] => Some((*x, *y, 0.0)),
                    _ => None,
                };
            }
        } else if tag.starts_with("</g") {
            transform = None;
        } else if in_defs {
            // marker defs: skip
        } else if tag.starts_with("<rect") {
            // x/y default to 0 per the SVG spec — a renderer that omits them
            // must not abort the whole parse.
            let (x, y, w, h) = (
                attr_f32(tag, "x").unwrap_or(0.0),
                attr_f32(tag, "y").unwrap_or(0.0),
                attr_f32(tag, "width")?,
                attr_f32(tag, "height")?,
            );
            let is_background =
                x <= 0.01 && y <= 0.01 && (w - svg_w).abs() < 2.0 && (h - svg_h).abs() < 2.0;
            let is_edge_label_bg = attr(tag, "data-edge-id").is_some()
                || attr(tag, "fill-opacity").is_some_and(|v| v.starts_with("0"));
            if !is_background && !is_edge_label_bg {
                elements.push(Element::NodeRect { x, y, w, h });
            }
        } else if tag.starts_with("<polygon") {
            let pts: Vec<(f32, f32)> = attr(tag, "points")?
                .split_whitespace()
                .filter_map(|p| p.split_once(','))
                .filter_map(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                .collect();
            if pts.len() >= 3 {
                let max_abs = pts
                    .iter()
                    .map(|(x, y)| x.abs().max(y.abs()))
                    .fold(0.0f32, f32::max);
                if max_abs < 20.0 {
                    // Arrowhead marker at origin — position from the <g>
                    // (the rotation is ignored: the arrow glyph is derived
                    // from the edge's terminal segment instead).
                    if let Some((tx, ty, _deg)) = transform {
                        elements.push(Element::Arrow { x: tx, y: ty });
                    }
                } else {
                    elements.push(Element::NodePoly { points: pts });
                }
            }
        } else if tag.starts_with("<ellipse") {
            elements.push(Element::NodeEllipse {
                cx: attr_f32(tag, "cx")?,
                cy: attr_f32(tag, "cy")?,
                rx: attr_f32(tag, "rx")?,
                ry: attr_f32(tag, "ry")?,
            });
        } else if tag.starts_with("<path") {
            let is_edge = attr(tag, "class").is_some_and(|c| c.contains("edgePath"))
                || attr(tag, "id").is_some_and(|id| id.starts_with("edge-"));
            if is_edge && let Some(d) = attr(tag, "d") {
                let points = sample_path(&d);
                if points.len() >= 2 {
                    elements.push(Element::Edge { points });
                }
            }
        } else if tag.starts_with("<text") {
            let x = attr_f32(tag, "x")?;
            let y = attr_f32(tag, "y")?;
            // Content up to </text>, stripping inner tags (tspan etc.).
            let Some(close) = rest.find("</text>") else {
                break;
            };
            let mut content = String::new();
            let mut inner = &rest[end..close];
            while let Some(lt) = inner.find('<') {
                content.push_str(&inner[..lt]);
                let Some(gt) = inner.find('>') else { break };
                inner = &inner[gt + 1..];
            }
            content.push_str(inner);
            let text = decode_entities(content.trim());
            if !text.is_empty() {
                elements.push(Element::Label { x, y, text });
            }
        }
        rest = &rest[end.min(rest.len())..];
    }
    if elements.is_empty() {
        None
    } else {
        Some(elements)
    }
}

/// Canvas cell: either a Braille dot mask (smooth curves) or a hard
/// character (frames, arrows, labels). Hard characters always win — a cell
/// carrying text or a frame border never shows stray curve dots.
#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Empty,
    Braille(u8),
    Char(char),
}

/// Braille dot bit for sub-position (sx % 2, sy % 4) within a cell.
fn braille_bit(sx: i32, sy: i32) -> u8 {
    match (sx.rem_euclid(2), sy.rem_euclid(4)) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        (0, 3) => 0x40,
        _ => 0x80,
    }
}

/// Character canvas with a Braille sub-cell layer (2×4 dots per cell) and
/// CJK-aware hard-character writes.
struct Canvas {
    cols: usize,
    rows: usize,
    cells: Vec<Vec<Cell>>,
}

impl Canvas {
    fn new(cols: usize, rows: usize) -> Self {
        Canvas {
            cols: cols.max(1),
            rows: rows.max(1),
            cells: vec![vec![Cell::Empty; cols.max(1)]; rows.max(1)],
        }
    }

    /// Set a Braille dot at sub-cell position (2·cols × 4·rows grid).
    /// Hard-character cells are left untouched.
    fn dot(&mut self, sx: i32, sy: i32) {
        if sx < 0 || sy < 0 {
            return;
        }
        let (c, r) = ((sx / 2) as usize, (sy / 4) as usize);
        if c >= self.cols || r >= self.rows {
            return;
        }
        let bit = braille_bit(sx, sy);
        match self.cells[r][c] {
            Cell::Empty => self.cells[r][c] = Cell::Braille(bit),
            Cell::Braille(m) => self.cells[r][c] = Cell::Braille(m | bit),
            Cell::Char(_) => {}
        }
    }

    /// Draw a polyline through sub-cell points (one dot per step).
    fn polyline(&mut self, pts: &[(f32, f32)]) {
        for pair in pts.windows(2) {
            let ((ax, ay), (bx, by)) = (pair[0], pair[1]);
            let steps = ((bx - ax).abs().max((by - ay).abs()).ceil() as i32).max(1);
            for i in 0..=steps {
                let t = i as f32 / steps as f32;
                self.dot(
                    (ax + (bx - ax) * t).round() as i32,
                    (ay + (by - ay) * t).round() as i32,
                );
            }
        }
    }

    fn put(&mut self, col: i32, row: i32, ch: char) {
        if col >= 0 && row >= 0 && (col as usize) < self.cols && (row as usize) < self.rows {
            self.cells[row as usize][col as usize] = Cell::Char(ch);
        }
    }

    /// Erase a cell (back to empty) — used to wipe curve dots from node
    /// interiors before frames and labels are drawn.
    fn clear(&mut self, col: i32, row: i32) {
        if col >= 0 && row >= 0 && (col as usize) < self.cols && (row as usize) < self.rows {
            self.cells[row as usize][col as usize] = Cell::Empty;
        }
    }

    fn text(&mut self, col: usize, row: usize, text: &str) {
        let mut c = col as i32;
        for ch in text.chars() {
            if c as usize >= self.cols {
                break;
            }
            self.put(c, row as i32, ch);
            // CJK/wide chars occupy two cells; blank the continuation.
            let w = tack_tui::line::grapheme_width(ch.to_string().as_str());
            if w == 2 {
                self.put(c + 1, row as i32, '\u{0}');
                c += 1;
            }
            c += 1;
        }
    }

    /// All cells in [col, col+len) at `row` empty (and inside the canvas)?
    /// Used to place edge labels without overwriting frames or curve dots.
    fn row_free(&self, col: usize, row: usize, len: usize) -> bool {
        if row >= self.rows || col + len > self.cols {
            return false;
        }
        self.cells[row][col..col + len]
            .iter()
            .all(|c| *c == Cell::Empty)
    }

    fn into_lines(self) -> Vec<tack_tui::Line> {
        let mut lines: Vec<tack_tui::Line> = self
            .cells
            .into_iter()
            .map(|row| {
                let s: String = row
                    .into_iter()
                    .map(|c| match c {
                        Cell::Empty => ' ',
                        Cell::Braille(m) => char::from_u32(0x2800 + m as u32).unwrap_or(' '),
                        Cell::Char(ch) => ch,
                    })
                    .collect::<String>()
                    .replace('\u{0}', "");
                tack_tui::Line::plain(s.trim_end().to_string())
            })
            .collect();
        // Trailing all-blank rows (canvas is sized from the SVG, content may
        // end higher) are dead scrollback space.
        while lines.last().is_some_and(|l| l.text().trim().is_empty()) {
            lines.pop();
        }
        lines
    }
}

/// Render the mermaid SVG as a Unicode text diagram sized to `width` cells.
pub fn render_text_art(svg: &str, width: usize) -> Option<Vec<tack_tui::Line>> {
    let elements = parse_svg(svg)?;
    // Canvas extent from the SVG dimensions.
    let tag_end = svg.find('>')?;
    let svg_w = attr_f32(&svg[..tag_end + 1], "width")?;
    let svg_h = attr_f32(&svg[..tag_end + 1], "height")?;
    // Non-finite (1e40 → inf) or non-positive dimensions are nonsense; rows
    // derived from them can saturate usize and abort on allocation.
    if !svg_w.is_finite() || !svg_h.is_finite() || svg_w <= 0.0 || svg_h <= 0.0 {
        return None;
    }
    let cols = width.clamp(20, 140);
    let sx = (cols - 1) as f32 / svg_w;
    // Cell aspect is ~1:2; the 4× vertical dot resolution of the Braille
    // layer keeps curves smooth at this compressed factor. Provisional —
    // the final sy is derived from snug frame heights further down.
    let sy = sx * 0.5;

    // Snug frames: the SVG pads nodes for bitmap fonts (bloat in cell space).
    // Each node's frame is sized around its label in CELLS, keeping the
    // layout's center. Boxes stay 3 rows tall; diamonds grow with the label.
    struct SnugBox {
        cx: i32,
        cy: i32,
        half_w: i32,
        half_h: i32,
        diamond: bool,
        /// Original SVG px bounds (for associating labels to nodes).
        px: (f32, f32, f32, f32),
    }
    impl SnugBox {
        fn contains(&self, x: i32, y: i32) -> bool {
            if self.diamond {
                let dx = (x - self.cx).abs() as f32 / self.half_w.max(1) as f32;
                let dy = (y - self.cy).abs() as f32 / self.half_h.max(1) as f32;
                dx + dy <= 1.05
            } else {
                (x - self.cx).abs() <= self.half_w && (y - self.cy).abs() <= self.half_h
            }
        }
        fn contains_px(&self, x: f32, y: f32) -> bool {
            x >= self.px.0 && x <= self.px.2 && y >= self.px.1 && y <= self.px.3
        }
    }
    let node_bounds = |el: &Element| -> Option<(f32, f32, f32, f32, bool)> {
        // (px x0, y0, x1, y1, diamond)
        match el {
            Element::NodeRect { x, y, w, h } => Some((*x, *y, x + w, y + h, false)),
            Element::NodeEllipse { cx, cy, rx, ry } => {
                Some((cx - rx, cy - ry, cx + rx, cy + ry, false))
            }
            Element::NodePoly { points } => {
                let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for (px, py) in points {
                    x0 = x0.min(*px);
                    y0 = y0.min(*py);
                    x1 = x1.max(*px);
                    y1 = y1.max(*py);
                }
                Some((x0, y0, x1, y1, true))
            }
            _ => None,
        }
    };
    let all_bounds: Vec<(f32, f32, f32, f32, bool)> =
        elements.iter().filter_map(node_bounds).collect();
    // Subgraph clusters: a big rect containing another node's center well
    // INSIDE it (the margin keeps composite shapes out — a cylinder cap's
    // center sits ON the body rect's border, ±float dust). Clusters are
    // not nodes: drawn as container frames further down.
    const INSIDE_EPS: f32 = 5.0;
    let clusters: Vec<(f32, f32, f32, f32)> = all_bounds
        .iter()
        .filter(|b| {
            all_bounds.iter().any(|o| {
                !std::ptr::eq(*b, o) && {
                    let (cx, cy) = ((o.0 + o.2) / 2.0, (o.1 + o.3) / 2.0);
                    cx > b.0 + INSIDE_EPS
                        && cx < b.2 - INSIDE_EPS
                        && cy > b.1 + INSIDE_EPS
                        && cy < b.3 - INSIDE_EPS
                        && (o.2 - o.0) * (o.3 - o.1) < (b.2 - b.0) * (b.3 - b.1)
                }
            })
        })
        .map(|b| (b.0, b.1, b.2, b.3))
        .collect();
    let is_cluster = |px: (f32, f32, f32, f32)| {
        clusters
            .iter()
            .any(|c| c.0 == px.0 && c.1 == px.1 && c.2 == px.2 && c.3 == px.3)
    };
    let mut snug: Vec<SnugBox> = Vec::new();
    for &(px0, py0, px1, py1, diamond) in &all_bounds {
        if is_cluster((px0, py0, px1, py1)) {
            continue;
        }
        let label = elements.iter().find_map(|e| {
            if let Element::Label { x, y, text } = e
                && *x >= px0
                && *x <= px1
                && *y >= py0
                && *y <= py1
            {
                return Some(text.clone());
            }
            None
        });
        let (ccx, ccy) = ((px0 + px1) / 2.0 * sx, (py0 + py1) / 2.0 * sy);
        let (ccx, ccy) = (ccx.round() as i32, ccy.round() as i32);
        let label_cells = label
            .as_deref()
            .map(tack_tui::line::grapheme_width)
            .unwrap_or(4) as i32;
        let (half_w, half_h) = if diamond {
            (label_cells / 2 + 3, (label_cells / 4 + 2).max(2))
        } else {
            ((label_cells / 2 + 2).max(3), 1)
        };
        snug.push(SnugBox {
            cx: ccx,
            cy: ccy,
            half_w,
            half_h,
            diamond,
            px: (px0, py0, px1, py1),
        });
    }
    // Composite shapes (a cylinder parses as a rect plus two ellipse caps,
    // …) produce overlapping snug boxes — drawing them all nests frames
    // inside each other. Drop any box whose center sits inside a bigger
    // one (strict px-containment misses caps that bulge past the body).
    let mut drop = vec![false; snug.len()];
    for i in 0..snug.len() {
        for j in 0..snug.len() {
            if i == j {
                continue;
            }
            let (a, b) = (snug[i].px, snug[j].px);
            let (ax, ay) = ((a.0 + a.2) / 2.0, (a.1 + a.3) / 2.0);
            let center_in_b = ax >= b.0 && ax <= b.2 && ay >= b.1 && ay <= b.3;
            let (area_a, area_b) = ((a.2 - a.0) * (a.3 - a.1), (b.2 - b.0) * (b.3 - b.1));
            if center_in_b && (area_a < area_b || (area_a == area_b && i < j)) {
                drop[i] = true;
            }
        }
    }
    let mut keep = 0;
    snug.retain(|_| {
        let k = !drop[keep];
        keep += 1;
        k
    });

    // Vertical density: snug frames are a constant 3 rows tall regardless
    // of zoom, so sy should map the px node height to those rows — using
    // the width-fit factor (sy = sx/2) instead turns narrow TD diagrams
    // into a dozen empty rows per rank gap (wide LR diagrams are naturally
    // dense, which is why only TD suffered).
    let cy_px = |b: &SnugBox| (b.px.1 + b.px.3) / 2.0;
    let mut heights: Vec<f32> = snug
        .iter()
        .map(|b| b.px.3 - b.px.1)
        .filter(|h| h.is_finite() && *h > 1.0)
        .collect();
    heights.sort_by(f32::total_cmp);
    let med_h = heights.get(heights.len() / 2).copied().unwrap_or(0.0);
    let mut sy = if med_h > 0.0 { 3.0 / med_h } else { sx * 0.5 };
    // Feasibility floor: x-overlapping frames of ADJACENT ranks (rank =
    // cy-cluster) must not overlap; keep 3 rows between borders (arrow,
    // curve fan-out, edge label). This RAISES sy when nodes are tightly
    // packed — it can never crush the diagram the way a min() cap did.
    let rank_gap = (med_h * 0.5).max(8.0);
    let mut rank_centers: Vec<f32> = snug.iter().map(&cy_px).collect();
    rank_centers.sort_by(f32::total_cmp);
    rank_centers.dedup_by(|a, b| (*a - *b).abs() <= rank_gap);
    let rank_of = |cy: f32| rank_centers.iter().filter(|&&c| c < cy - rank_gap).count();
    let x_overlap = |a: &SnugBox, b: &SnugBox| {
        !(a.cx - a.half_w > b.cx + b.half_w + 1 || b.cx - b.half_w > a.cx + a.half_w + 1)
    };
    for i in 0..snug.len() {
        for j in (i + 1)..snug.len() {
            let (a, b) = (&snug[i], &snug[j]);
            if !x_overlap(a, b) || rank_of(cy_px(a)).abs_diff(rank_of(cy_px(b))) != 1 {
                continue;
            }
            let pitch_px = (cy_px(a) - cy_px(b)).abs();
            if !pitch_px.is_finite() || pitch_px < 1.0 {
                continue;
            }
            let need_rows = (a.half_h + b.half_h + 3) as f32;
            sy = sy.max(need_rows / pitch_px);
        }
    }
    // Re-map centers with the final sy (cx is sx-derived, hence stable).
    for b in &mut snug {
        b.cy = (((b.px.1 + b.px.3) / 2.0) * sy).round() as i32;
    }
    // Cap the canvas: an extreme aspect ratio must not allocate gigabytes.
    let rows = ((svg_h * sy).ceil().max(2.0) as usize).min(512);
    let mut canvas = Canvas::new(cols, rows);
    let map = |x: f32, y: f32| (x * sx, y * sy); // px → cell coords
    let sub = |x: f32, y: f32| (x * sx * 2.0, y * sy * 4.0); // px → sub-cell
    // Arrowhead markers from the SVG (px positions at edge tips): an edge
    // end only gets an arrow when a marker sits next to it, so `---` edges
    // stay arrowless.
    let arrow_tips: Vec<(f32, f32)> = elements
        .iter()
        .filter_map(|e| {
            if let Element::Arrow { x, y, .. } = e {
                Some((*x, *y))
            } else {
                None
            }
        })
        .collect();
    let tip_tol = (svg_w.max(svg_h) * 0.03).clamp(6.0, 24.0);

    // Walk an edge end (sub-cell coords) toward the node it approaches
    // until the snug frame is touched. Returns the replacement tail points
    // (in polyline order), the frame cell first hit, and the final approach
    // direction (for the arrow glyph).
    //
    // The snug frame is label-sized while the SVG attaches edges to the
    // padded bitmap box — at small diagrams/wide terminals the two differ
    // by many cells, so walking blindly along the edge direction can miss
    // the frame entirely (floating arrows / lines into the void). Then we
    // steer: the target node is known (the edge's px end sits on its
    // border). An edge entering ALONG its axis from outside gets an
    // L-shaped correction (preserving box-drawing rendering); anything
    // else approaches the center straight, stopping at the border — an
    // L-bend from the side would cut across the box interior.
    /// Edge-end extension result: replacement tail points (in polyline
    /// order), the frame cell first hit, and the final approach direction
    /// (for the arrow glyph).
    type ExtendedEnd = (Vec<(f32, f32)>, Option<(i32, i32)>, (f32, f32));
    let cell_of = |p: (f32, f32)| ((p.0 / 2.0).round() as i32, (p.1 / 4.0).round() as i32);
    let extend_end =
        |p: (f32, f32), dir: (f32, f32), px_end: (f32, f32), snug: &[SnugBox]| -> ExtendedEnd {
            let hit_cell = |p: (f32, f32)| -> Option<(i32, i32)> {
                let c = cell_of(p);
                snug.iter().any(|b| b.contains(c.0, c.1)).then_some(c)
            };
            let walk = |mut cur: (f32, f32), d: (f32, f32), steps: usize| {
                let len = (d.0 * d.0 + d.1 * d.1).sqrt().max(0.001);
                let (ux, uy) = (d.0 / len, d.1 / len);
                for _ in 0..steps {
                    cur = (cur.0 + ux, cur.1 + uy);
                    if let Some(c) = hit_cell(cur) {
                        return (cur, Some(c));
                    }
                }
                (cur, None)
            };
            if let Some(c) = hit_cell(p) {
                return (vec![p], Some(c), dir);
            }
            let (hit_p, hit) = walk(p, dir, 60);
            if hit.is_some() {
                return (vec![hit_p], hit, dir);
            }
            // No px association → give up (leave the endpoint untouched rather
            // than drawing phantom lines into empty canvas).
            let Some(t) = snug.iter().find(|b| b.contains_px(px_end.0, px_end.1)) else {
                return (vec![p], None, dir);
            };
            let center = (t.cx as f32 * 2.0 + 0.5, t.cy as f32 * 4.0 + 1.5);
            let vertical = dir.1.abs() > dir.0.abs();
            // Beyond = the endpoint lies past the frame on the travel axis;
            // perp = sideways offset from the center (both in sub-cells).
            let (beyond, perp) = if vertical {
                (
                    (p.1 - center.1).abs() > t.half_h as f32 * 4.0 + 2.0,
                    (p.0 - center.0).abs(),
                )
            } else {
                (
                    (p.0 - center.0).abs() > t.half_w as f32 * 2.0 + 2.0,
                    (p.1 - center.1).abs(),
                )
            };
            let axis_dir = if vertical && dir.1 != 0.0 {
                Some((0.0, dir.1.signum()))
            } else if !vertical && dir.0 != 0.0 {
                Some((dir.0.signum(), 0.0))
            } else {
                None
            };
            match (beyond && perp <= 30.0, axis_dir) {
                (true, Some(d)) => {
                    let bend = if vertical {
                        (center.0, p.1)
                    } else {
                        (p.0, center.1)
                    };
                    let (f, hit) = walk(bend, d, 400);
                    (vec![bend, f], hit.or(Some(cell_of(f))), d)
                }
                _ => {
                    let d = (center.0 - p.0, center.1 - p.1);
                    let (f, hit) = walk(p, d, 400);
                    (vec![f], hit.or(Some(cell_of(f))), d)
                }
            }
        };

    struct EdgeArt {
        /// Sub-cell polyline, both ends extended to the node frames.
        pts: Vec<(f32, f32)>,
        /// Orthogonal (straight / 90° bends) edges render as box characters.
        orthogonal: bool,
        /// Arrowhead cell + glyph at the target end, if the SVG had a marker.
        arrow: Option<(i32, i32, char)>,
    }
    let mut edges: Vec<EdgeArt> = Vec::new();
    for el in &elements {
        let Element::Edge { points } = el else {
            continue;
        };
        if points.len() < 2 {
            continue;
        }
        let n = points.len();
        let mut pts: Vec<(f32, f32)> = points.iter().map(|(x, y)| sub(*x, *y)).collect();
        // Trim sampled points that fall INSIDE a node frame. Snug frames
        // are label-sized and can exceed the mapped px shape at small
        // scales (a diamond's px vertices land INSIDE its snug outline),
        // and some emitters draw paths from the node center — either way
        // the interior segment must not render, or the line reads as
        // "starting from the middle of the diamond".
        let inside_any = |p: (f32, f32)| {
            let c = cell_of(p);
            snug.iter().any(|b| b.contains(c.0, c.1))
        };
        let mut start = 0;
        while start + 1 < pts.len() && inside_any(pts[start]) {
            start += 1;
        }
        let mut end = pts.len();
        while end > start + 2 && inside_any(pts[end - 1]) {
            end -= 1;
        }
        pts.drain(end..);
        pts.drain(..start);
        if pts.len() < 2 {
            continue;
        }
        // extend_end returns the tail in walk order (bend first, frame
        // last); the polyline STARTS at the frame, so reverse for the head.
        let (mut tail0, _, _) = extend_end(
            pts[0],
            (pts[0].0 - pts[1].0, pts[0].1 - pts[1].1),
            points[0],
            &snug,
        );
        tail0.reverse();
        pts.splice(..1, tail0);
        let m = pts.len();
        let (tailn, hit, arrow_dir) = extend_end(
            pts[m - 1],
            (pts[m - 1].0 - pts[m - 2].0, pts[m - 1].1 - pts[m - 2].1),
            points[n - 1],
            &snug,
        );
        pts.splice(m - 1.., tailn);
        // Orthogonal routes have few waypoints and only axis-aligned
        // segments (L-shaped end corrections keep it so); Bézier-sampled
        // curves (12+ points) go to the Braille layer even when locally
        // flat (a gentle S must not become a ─│──│ staircase).
        let axis_eps = 0.6f32;
        let orthogonal = pts.len() <= 8
            && pts.windows(2).all(|w| {
                (w[1].0 - w[0].0).abs() <= axis_eps || (w[1].1 - w[0].1).abs() <= axis_eps
            });
        let arrow = if arrow_tips
            .iter()
            .any(|(ax, ay)| (ax - points[n - 1].0).hypot(ay - points[n - 1].1) <= tip_tol)
        {
            // Direction from the edge's actual final approach (the SVG
            // marker angle no longer matches after polyline rasterization).
            let (dx, dy) = arrow_dir;
            let glyph = if dx.abs() >= dy.abs() {
                if dx > 0.0 { '▶' } else { '◀' }
            } else if dy > 0.0 {
                '▼'
            } else {
                '▲'
            };
            let last = pts[pts.len() - 1];
            let mut cell =
                hit.unwrap_or(((last.0 / 2.0).round() as i32, (last.1 / 4.0).round() as i32));
            // Keep arrows off corner cells: an arrow overwriting ┌┐└┘ loses
            // both border lines (worse with several edges into one node).
            // Slide along the border toward the center instead.
            if let Some(b) = snug
                .iter()
                .find(|b| !b.diamond && b.contains(cell.0, cell.1))
            {
                if (cell.1 - b.cy).abs() == b.half_h && b.half_w > 1 {
                    cell.0 = cell.0.clamp(b.cx - b.half_w + 1, b.cx + b.half_w - 1);
                }
                if (cell.0 - b.cx).abs() == b.half_w {
                    cell.1 = cell.1.clamp(b.cy - b.half_h + 1, b.cy + b.half_h - 1);
                }
            }
            Some((cell.0, cell.1, glyph))
        } else {
            None
        };
        edges.push(EdgeArt {
            pts,
            orthogonal,
            arrow,
        });
    }

    // Pass 1: edges. Orthogonal ones as box characters with corner pieces at
    // bends, curves on the Braille sub-cell layer.
    let corner = |din: (f32, f32), dout: (f32, f32)| -> char {
        let dir = |d: (f32, f32)| -> u8 {
            if d.0.abs() > d.1.abs() {
                if d.0 > 0.0 { b'R' } else { b'L' }
            } else if d.1 > 0.0 {
                b'D'
            } else {
                b'U'
            }
        };
        match (dir(din), dir(dout)) {
            (b'R', b'D') | (b'U', b'L') => '┐',
            (b'R', b'U') | (b'D', b'L') => '┘',
            (b'L', b'D') | (b'U', b'R') => '┌',
            _ => '└',
        }
    };
    for e in &edges {
        if !e.orthogonal {
            canvas.polyline(&e.pts);
            continue;
        }
        let cell_of = |p: (f32, f32)| ((p.0 / 2.0).round() as i32, (p.1 / 4.0).round() as i32);
        for (i, w) in e.pts.windows(2).enumerate() {
            let (a, b) = (cell_of(w[0]), cell_of(w[1]));
            if (b.0 - a.0).abs() >= (b.1 - a.1).abs() {
                for x in a.0.min(b.0)..=a.0.max(b.0) {
                    canvas.put(x, a.1, '─');
                }
            } else {
                for y in a.1.min(b.1)..=a.1.max(b.1) {
                    canvas.put(a.0, y, '│');
                }
            }
            if i + 2 < e.pts.len() {
                let joint = cell_of(e.pts[i + 1]);
                let din = (e.pts[i + 1].0 - e.pts[i].0, e.pts[i + 1].1 - e.pts[i].1);
                let dout = (
                    e.pts[i + 2].0 - e.pts[i + 1].0,
                    e.pts[i + 2].1 - e.pts[i + 1].1,
                );
                canvas.put(joint.0, joint.1, corner(din, dout));
            }
        }
    }

    // Pass 2: diamond frames (Braille — a diamond's slopes degenerate into
    // ─/╲ staircases on a plain char grid).
    for b in &snug {
        if !b.diamond {
            continue;
        }
        let sc = |x: f32, y: f32| (x * 2.0 + 0.5, y * 4.0 + 1.5); // cell center
        let (cx, cy, hw, hh) = (b.cx as f32, b.cy as f32, b.half_w as f32, b.half_h as f32);
        let top = sc(cx, cy - hh);
        let right = sc(cx + hw, cy);
        let bottom = sc(cx, cy + hh);
        let left = sc(cx - hw, cy);
        canvas.polyline(&[top, right, bottom, left, top]);
    }

    // Pass 2.5: wipe Braille dots strictly inside node interiors — edges
    // routed under a node (or over-trimmed) would otherwise leave dots
    // reading as lines through the frame.
    for b in &snug {
        for r in (b.cy - b.half_h)..=(b.cy + b.half_h) {
            for c in (b.cx - b.half_w)..=(b.cx + b.half_w) {
                let inside = if b.diamond {
                    let dx = (c - b.cx).abs() as f32 / b.half_w.max(1) as f32;
                    let dy = (r - b.cy).abs() as f32 / b.half_h.max(1) as f32;
                    dx + dy < 0.7
                } else {
                    (c - b.cx).abs() < b.half_w && (r - b.cy).abs() < b.half_h
                };
                if inside {
                    canvas.clear(c, r);
                }
            }
        }
    }

    // Pass 3: subgraph cluster frames (container rects from the layout,
    // full px bounds) — before node frames so nested nodes draw over them.
    for &(px0, py0, px1, py1) in &clusters {
        let (x0, y0) = (map(px0, py0), map(px1, py1));
        let (x0, y0, x1, y1) = (
            x0.0.round() as i32,
            x0.1.round() as i32,
            y0.0.round() as i32,
            y0.1.round() as i32,
        );
        for cx in x0..=x1 {
            canvas.put(cx, y0, '─');
            canvas.put(cx, y1, '─');
        }
        for cy in y0..=y1 {
            canvas.put(x0, cy, '│');
            canvas.put(x1, cy, '│');
        }
        canvas.put(x0, y0, '┌');
        canvas.put(x1, y0, '┐');
        canvas.put(x0, y1, '└');
        canvas.put(x1, y1, '┘');
    }

    // Pass 4: rectangular node frames as box characters.
    for b in &snug {
        if b.diamond {
            continue;
        }
        let (x0, y0) = ((b.cx - b.half_w).max(0), (b.cy - b.half_h).max(0));
        let (x1, y1) = (b.cx + b.half_w, b.cy + b.half_h);
        for cx in x0..=x1 {
            canvas.put(cx, y0, '─');
            canvas.put(cx, y1, '─');
        }
        for cy in y0..=y1 {
            canvas.put(x0, cy, '│');
            canvas.put(x1, cy, '│');
        }
        canvas.put(x0, y0, '┌');
        canvas.put(x1, y0, '┐');
        canvas.put(x0, y1, '└');
        canvas.put(x1, y1, '┘');
    }

    // Pass 5: arrowheads — they sit ON the target frame border, so they
    // overdraw it (an arrow touching the box reads as connected).
    for e in &edges {
        if let Some((c, r, g)) = e.arrow {
            canvas.put(c, r, g);
        }
    }

    // Pass 6: labels. Node labels center on their snug frame (the SVG y is a
    // text baseline, which lands below the cell center); cluster labels sit
    // just inside their container frame; edge labels are glued to the
    // nearest edge point — the SVG offsets them for bitmap scales, which
    // lands far from the line at cell scale.
    for el in &elements {
        if let Element::Label { x, y, text } = el {
            let text_w: usize = tack_tui::line::grapheme_width(text);
            if let Some(b) = snug.iter().find(|b| b.contains_px(*x, *y)) {
                let col = (b.cx as f32 - text_w as f32 / 2.0).round().max(0.0) as usize;
                canvas.text(col, b.cy.max(0) as usize, text);
            } else if let Some(&c) = clusters
                .iter()
                .find(|c| *x >= c.0 && *x <= c.2 && *y >= c.1 && *y <= c.3)
            {
                let (ccx, ccy) = map((c.0 + c.2) / 2.0, *y);
                let col = (ccx - text_w as f32 / 2.0).round().max(0.0) as usize;
                canvas.text(col, ccy.round().max(0.0) as usize, text);
            } else {
                let (lx, ly) = sub(*x, *y);
                let mut best: Option<(f32, f32, f32)> = None; // (dist², sx, sy)
                for e in &edges {
                    for p in &e.pts {
                        let d2 = (p.0 - lx).powi(2) + (p.1 - ly).powi(2);
                        if best.is_none_or(|(bd, _, _)| d2 < bd) {
                            best = Some((d2, p.0, p.1));
                        }
                    }
                }
                let (ccx, ccy) = best
                    .map(|(_, px, py)| (px / 2.0, py / 4.0))
                    .unwrap_or((lx / 2.0, ly / 4.0));
                let (cx_i, cy_i) = (ccx.round() as i32, ccy.round() as i32);
                let w = text_w as i32;
                let centered = (cx_i - w / 2).max(0);
                // First fully free placement wins (above → below → right →
                // left of the nearest edge point), so the label never
                // overwrites frames, arrows or the curve itself.
                let (col, row) = [
                    (centered, cy_i - 1),
                    (centered, cy_i + 1),
                    (cx_i + 2, cy_i),
                    (cx_i - w - 1, cy_i),
                ]
                .into_iter()
                .find(|(c, r)| {
                    *r >= 0 && *c >= 0 && canvas.row_free(*c as usize, *r as usize, w as usize)
                })
                // Every candidate taken (cramped branch area): scan further
                // above/below — a label further from its line beats a
                // dropped or overwritten one.
                .or_else(|| {
                    (2..=6)
                        .flat_map(|d| [(centered, cy_i - d), (centered, cy_i + d)])
                        .find(|(c, r)| {
                            *r >= 0 && canvas.row_free(*c as usize, *r as usize, w as usize)
                        })
                })
                .unwrap_or((centered, (cy_i - 1).max(0)));
                canvas.text(col as usize, row as usize, text);
            }
        }
    }
    Some(canvas.into_lines())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn art_text(lines: &[tack_tui::Line]) -> String {
        lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rect_without_xy_defaults_to_origin() {
        // SVG specifies x/y default to 0; a renderer that omits them must not
        // abort the whole parse (text art silently falling back to bitmap).
        let svg = r#"<svg width="200" height="100"><rect y="10" width="80" height="30"/><text x="20" y="30">Hello</text></svg>"#;
        let lines = render_text_art(svg, 80).expect("art");
        let text = art_text(&lines);
        assert!(text.contains("Hello"), "{text}");
    }

    #[test]
    fn insane_svg_dimensions_do_not_panic_or_abort() {
        // height=1e40 parses to f32::INFINITY: rows = (inf * sy) as usize
        // saturates to usize::MAX → vec! capacity overflow. Must bail out.
        let svg = r#"<svg width="100" height="1e40"><rect x="0" y="0" width="50" height="20"/><text x="5" y="10">Hi</text></svg>"#;
        let _ = render_text_art(svg, 80);
        // Extreme but finite aspect ratio: rows must be capped.
        let svg = r#"<svg width="1" height="100000000"><rect x="0" y="0" width="1" height="10"/><text x="0" y="5">Hi</text></svg>"#;
        if let Some(lines) = render_text_art(svg, 80) {
            assert!(lines.len() <= 1024, "{} rows", lines.len());
        }
    }

    #[test]
    fn path_parser_handles_svg_number_edge_cases() {
        // "10-20" (no comma, sign as separator) and scientific notation used
        // to mis-tokenize into garbage coordinates — the phantom full-height
        // lines in text art.
        let pts = sample_path("M10-20 L1e2 30");
        assert_eq!(pts, vec![(10.0, -20.0), (100.0, 30.0)]);
        // H/V and relative commands.
        let pts = sample_path("M0 0 h10 v5 l-5 0");
        assert_eq!(pts, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 5.0), (5.0, 5.0)]);
        // Smooth cubic S reflects the previous control point.
        let pts = sample_path("M0 0 C0 10 10 10 10 20 S20 30 20 40");
        assert_eq!(pts.first().copied(), Some((0.0, 0.0)));
        assert_eq!(pts.last().copied(), Some((20.0, 40.0)));
    }

    #[test]
    fn flowchart_becomes_text_art() {
        let svg = mermaid_rs_renderer::render("flowchart LR; A[浏览器] --> B[HTTP 请求]").unwrap();
        let lines = render_text_art(&svg, 80).expect("art");
        let text = art_text(&lines);
        assert!(text.contains("浏览器"), "{text}");
        assert!(text.contains("HTTP 请求"), "{text}");
        assert!(text.contains('┌'), "{text}");
        assert!(text.contains('▶'), "{text}");
        // A straight LR edge renders as a solid box line, not dotted Braille.
        assert!(text.contains("──"), "{text}");
    }

    #[test]
    fn no_arrow_for_open_edges() {
        // `---` has no arrowhead marker: the art must not invent one.
        let svg = mermaid_rs_renderer::render("flowchart LR; A[甲] --- B[乙]").unwrap();
        let lines = render_text_art(&svg, 60).expect("art");
        let text = art_text(&lines);
        assert!(!text.contains('▶'), "{text}");
        assert!(!text.contains('◀'), "{text}");
    }

    #[test]
    fn no_phantom_content_below_last_node() {
        // Unbounded edge-end extension used to draw vertical lines into the
        // void below the last node.
        let svg = crate::tui::mermaid::render_svg_for_test(
            "flowchart TD\n A[开始] --> B{条件}\n B -->|是| C[处理]\n B -->|否| D[跳过]\n C --> E[保存]\n D --> E\n E --> F[结束]",
        )
        .unwrap();
        let lines = render_text_art(&svg, 80).expect("art");
        let last = lines.last().map(|l| l.text()).unwrap_or_default();
        assert!(
            last.contains('└') || last.contains('┘') || last.contains('⠈') || last.contains('⠉'),
            "last line is the bottom of the final node frame: {last:?}\n{}",
            art_text(&lines)
        );
    }

    #[test]
    fn diamond_and_labels_render() {
        let svg = crate::tui::mermaid::render_svg_for_test(
            "flowchart TB\n A[开始] --> B{条件}\n B -->|是| C[处理]\n B -->|否| D[结束]",
        )
        .unwrap();
        let lines = render_text_art(&svg, 80).expect("art");
        eprintln!("\n{}", art_text(&lines));
        let text = art_text(&lines);
        assert!(text.contains("开始"), "{text}");
        assert!(text.contains("条件"), "{text}");
        assert!(text.contains("是"), "{text}");
        // Edge labels sit next to their edge, not at the canvas fringe:
        // every label row must also contain an edge/frame glyph or be
        // directly adjacent to one.
        let pos = text.find('是').unwrap();
        let line_start = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = text[pos..]
            .find('\n')
            .map(|i| pos + i)
            .unwrap_or(text.len());
        let label_row = &text[line_start..line_end];
        assert!(
            label_row.chars().any(|c| "─│┌┐└┘▼▲◀▶".contains(c))
                || label_row
                    .chars()
                    .any(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
            "edge label row has diagram content nearby: {label_row:?}"
        );
    }
}

#[cfg(test)]
mod subgraph_tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn subgraph_renders_container_children_and_label() {
        let svg = crate::tui::mermaid::render_svg_for_test(
            "flowchart TB\n  subgraph Inner[处理层]\n    B[解析] --> C[执行]\n  end\n  A[入口] --> B\n  C --> D[输出]",
        )
        .unwrap();
        let lines = super::render_text_art(&svg, 90).expect("art");
        let text: String = lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n");
        // Children survive (container dedupe must not eat them), the cluster
        // label is drawn once, and edges attach with arrowheads.
        assert!(text.contains("解析"), "{text}");
        assert!(text.contains("执行"), "{text}");
        assert!(text.contains("入口"), "{text}");
        assert!(text.contains("输出"), "{text}");
        assert_eq!(text.matches("处理层").count(), 1, "{text}");
        assert!(text.contains('▼'), "{text}");
        // Labels must never overlap into glyph hash (处理层 + 执行 collision).
        assert!(!text.contains("处执行"), "{text}");
    }

    #[test]
    fn cylinder_renders_as_single_frame() {
        let svg =
            crate::tui::mermaid::render_svg_for_test("flowchart TB\n  A[上] --> D[(库)]").unwrap();
        let lines = super::render_text_art(&svg, 60).expect("art");
        let text: String = lines
            .iter()
            .map(|l| l.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("库"), "{text}");
        // Composite-shape dedupe: one frame, no nested ┌ inside └…┘ rows.
        assert_eq!(text.matches('┌').count(), 2, "{text}");
    }
}
