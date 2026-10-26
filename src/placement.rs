//! Where a module sits on the base, from the socket (USB port key) it's plugged into, and a drawing of it for
//! the settings page.
//!
//! The base has a 4×6 grid of cells (in its vertical orientation, column 0 / row 0 at the top left) and 27
//! sockets. A socket is described by where a 2×2 module (knobs, dial) plugged into it sits and how it's turned;
//! every other module sits in a fixed place beside that 2×2 one, turned with it. Modules may overhang the base.
//!
//! The socket table comes from the official app's per-socket tables (docs/masterhub-app-analysis.md), solved
//! for one position per socket. Its 91 sound entries all agree with it; the wrong ones (faders at 31 and 32,
//! dial at 121, rollers at 34 and 124) disagree with the other modules in the same socket and aren't used.

use crate::Kind;

const COLS: i8 = 4;
const ROWS: i8 = 6;

/// Cells a module covers in the base's vertical frame (may overhang: negative or past the grid) and its
/// clockwise rotation there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub col: i8,
    pub row: i8,
    pub cols: i8,
    pub rows: i8,
    pub rotation: u16,
}

/// Size (columns × rows) of each module, unturned.
fn footprint(kind: Kind) -> (i8, i8) {
    match kind {
        Kind::Keys => (4, 3),
        Kind::Faders => (4, 2),
        Kind::Knobs | Kind::Dial => (2, 2),
        Kind::Rollers => (4, 1),
    }
}

/// (socket, column, row, turn) of a 2×2 module plugged into the socket. "144" is the base's own controller.
const SOCKETS: &[(&str, i8, i8, u16)] = &[
    ("21", 2, 2, 180),
    ("22", 1, 2, 270),
    ("23", 2, 2, 270),
    ("24", 2, 1, 0),
    ("31", 0, 2, 0),
    ("32", 1, 2, 180),
    ("33", 0, 2, 180),
    ("34", 0, 0, 0),
    ("41", 2, 3, 0),
    ("42", 2, 2, 0),
    ("43", 2, 3, 270),
    ("44", 2, 4, 270),
    ("111", 0, 2, 270),
    ("112", 2, 1, 270),
    ("113", 2, 0, 0),
    ("114", 3, 2, 180),
    ("121", 2, 5, 270),
    ("122", 2, 4, 0),
    ("123", 0, 4, 0),
    ("124", 0, 4, 270),
    ("131", 2, 0, 180),
    ("132", 1, 0, 270),
    ("133", 3, 0, 180),
    ("134", 2, 0, 270),
    ("141", 0, 0, 180),
    ("142", 1, 0, 180),
    ("143", 0, 0, 270),
];

/// Where a module sits beside the 2×2 one with its top-left at (0, 0), both unturned: its cells (column, row,
/// columns, rows), and how much more it's turned (the rollers stand across).
fn beside(kind: Kind) -> ((i8, i8, i8, i8), u16) {
    match kind {
        Kind::Knobs | Kind::Dial => ((0, 0, 2, 2), 0),
        Kind::Keys => ((-2, -1, 4, 3), 0),
        Kind::Faders => ((-2, 0, 4, 2), 0),
        Kind::Rollers => ((1, -2, 1, 4), 90),
    }
}

/// The contacts of a 2×2 module, unturned: on its right edge, half a cell from the top (half cells).
const CONTACTS: (i8, i8) = (4, 1);

/// A point in half cells, turned `turn` degrees clockwise about the middle of the 2×2 module at (0, 0).
fn turned((x, y): (i8, i8), turn: u16) -> (i8, i8) {
    let (mut x, mut y) = (x - 2, y - 2);
    for _ in 0..turn / 90 {
        (x, y) = (-y, x);
    }
    (x + 2, y + 2)
}

fn socket(socket: &str) -> Option<(i8, i8, u16)> {
    SOCKETS.iter().find(|s| s.0 == socket).map(|&(_, c, r, t)| (c, r, t))
}

/// Cells and rotation of a `kind` module in `socket`; None for a socket that isn't on the base.
pub fn place(kind: Kind, socket_key: &str) -> Option<Placement> {
    let (col, row, turn) = socket(socket_key)?;
    let ((x, y, w, h), extra) = beside(kind);
    let a = turned((2 * x, 2 * y), turn);
    let b = turned((2 * (x + w), 2 * (y + h)), turn);
    Some(Placement {
        col: col + a.0.min(b.0) / 2,
        row: row + a.1.min(b.1) / 2,
        cols: (a.0 - b.0).abs() / 2,
        rows: (a.1 - b.1).abs() / 2,
        rotation: (turn + extra) % 360,
    })
}

/// Where a socket's contacts are on the base, in cells; None for a socket that isn't on the base.
fn contacts(socket_key: &str) -> Option<(f32, f32)> {
    let (col, row, turn) = socket(socket_key)?;
    let (x, y) = turned(CONTACTS, turn);
    Some((col as f32 + x as f32 / 2.0, row as f32 + y as f32 / 2.0))
}

/// A connected module for the drawing.
pub struct Shown<'a> {
    pub kind: Kind,
    pub label: &'a str,
    pub socket: Option<&'a str>,
}

/// The base and its modules as an SVG, turned `base_turn` degrees clockwise like the base on the desk: every
/// module on its cells with an arrow for its top, the sockets as small numbered dots.
pub fn svg(modules: &[Shown], base_turn: u16) -> String {
    const CELL: f32 = 56.0;
    // turn a point (cells, y down) clockwise by base_turn
    let turn = |x: f32, y: f32| match base_turn % 360 {
        90 => (-y, x),
        180 => (-x, -y),
        270 => (y, -x),
        _ => (x, y),
    };
    let turn_rect = |c: f32, r: f32, w: f32, h: f32| {
        let (a, b) = (turn(c, r), turn(c + w, r + h));
        (a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
    };
    let placed: Vec<(&Shown, Placement)> =
        modules.iter().filter_map(|m| Some((m, place(m.kind, m.socket?)?))).collect();
    // bounds of everything drawn, in turned cells
    let mut rects = vec![turn_rect(0.0, 0.0, COLS as f32, ROWS as f32)];
    rects.extend(placed.iter().map(|(_, p)| turn_rect(p.col as f32, p.row as f32, p.cols as f32, p.rows as f32)));
    // room around it for labels wider than a narrow module
    let x0 = rects.iter().map(|r| r.0).fold(f32::MAX, f32::min) - 1.0;
    let y0 = rects.iter().map(|r| r.1).fold(f32::MAX, f32::min) - 0.4;
    let x1 = rects.iter().map(|r| r.0 + r.2).fold(f32::MIN, f32::max) + 1.0;
    let y1 = rects.iter().map(|r| r.1 + r.3).fold(f32::MIN, f32::max) + 0.4;
    let px = |v: f32, o: f32| (v - o) * CELL;
    let mut out = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{:.0}' height='{:.0}' font-family='system-ui,sans-serif' style='max-width:100%;height:auto'>",
        (x1 - x0) * CELL,
        (y1 - y0) * CELL
    );
    // the base: cells and sockets
    for r in 0..ROWS {
        for c in 0..COLS {
            let (x, y, w, h) = turn_rect(c as f32, r as f32, 1.0, 1.0);
            out += &format!(
                "<rect x='{:.1}' y='{:.1}' width='{:.1}' height='{:.1}' fill='#2a2a30' stroke='#45454d'/>",
                px(x, x0),
                px(y, y0),
                w * CELL,
                h * CELL
            );
        }
    }
    for &(socket, ..) in SOCKETS {
        let (sx, sy) = contacts(socket).unwrap();
        let (x, y) = turn(sx, sy);
        let (x, y) = (px(x, x0), px(y, y0));
        out += &format!("<circle cx='{x:.1}' cy='{y:.1}' r='3' fill='#8a8a95'/><text x='{:.1}' y='{:.1}' font-size='9' fill='#8a8a95'>{socket}</text>", x + 4.0, y - 3.0);
    }
    // the modules
    for (m, p) in &placed {
        let (x, y, w, h) = turn_rect(p.col as f32, p.row as f32, p.cols as f32, p.rows as f32);
        let (x, y, w, h) = (px(x, x0), px(y, y0), w * CELL, h * CELL);
        let colour = match m.kind {
            Kind::Keys => "#2e8bff",
            Kind::Knobs => "#a05cff",
            Kind::Faders => "#00b383",
            Kind::Rollers => "#ff9f1a",
            Kind::Dial => "#ff4f7b",
        };
        let up = (p.rotation + base_turn) % 360; // where the module's top points as the user sees it
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        out += &format!(
            "<rect x='{:.1}' y='{:.1}' width='{:.1}' height='{:.1}' rx='6' fill='{colour}' fill-opacity='0.8' stroke='{colour}' stroke-width='2'/>\
             <text x='{cx:.1}' y='{:.1}' font-size='26' fill='#fff' text-anchor='middle' transform='rotate({up} {cx:.1} {cy:.1})'>↑</text>\
             <text x='{cx:.1}' y='{:.1}' font-size='12' font-weight='600' fill='#fff' text-anchor='middle'>{}</text>\
             <text x='{cx:.1}' y='{:.1}' font-size='11' fill='#ddd' text-anchor='middle'>socket {} · {up}°</text>",
            x + 2.0,
            y + 2.0,
            w - 4.0,
            h - 4.0,
            cy - 4.0,
            cy + 22.0,
            esc(m.label),
            cy + 36.0,
            m.socket.unwrap_or_default(),
        );
    }
    // the contacts each module is plugged into, over the module: the USB port it uses
    for (m, _) in &placed {
        if let Some((socket, (sx, sy))) = m.socket.and_then(|s| Some((s, contacts(s)?))) {
            let (x, y) = turn(sx, sy);
            let (x, y) = (px(x, x0), px(y, y0));
            out += &format!(
                "<g class=port><circle cx='{x:.1}' cy='{y:.1}' r='9' fill='#fff' stroke='#16161b' stroke-width='2'/>\
                 <circle cx='{x:.1}' cy='{y:.1}' r='4' fill='#16161b'/>\
                 <text x='{:.1}' y='{:.1}' font-size='11' font-weight='700' fill='#fff' stroke='#16161b' stroke-width='3' paint-order='stroke'>{socket}</text></g>",
                x + 11.0,
                y + 4.0
            );
        }
    }
    out + "</svg>"
}

/// The same view with pictures of the modules (the plugin's own, `module_picture` and `base_picture`): the base
/// and each module on their cells, turned like on the desk. Cell (c, r) sits at 25 + 129·c px of the base; a
/// module picture fills its cells less the 9 px gap between them.
pub fn pictures(modules: &[Shown], base_turn: u16) -> String {
    const PITCH: f32 = 129.0;
    const MARGIN: f32 = 25.0;
    const SCALE: f32 = 0.55;
    let turn = |x: f32, y: f32| match base_turn % 360 {
        90 => (-y, x),
        180 => (-x, -y),
        270 => (y, -x),
        _ => (x, y),
    };
    // a rectangle in base pixels (unturned) -> turned and axis-aligned
    let turn_rect = |x: f32, y: f32, w: f32, h: f32| {
        let (a, b) = (turn(x, y), turn(x + w, y + h));
        (a.0.min(b.0), a.1.min(b.1), (a.0 - b.0).abs(), (a.1 - b.1).abs())
    };
    let base = (0.0, 0.0, BASE_SIZE.0, BASE_SIZE.1);
    let placed: Vec<(&Shown, Placement)> =
        modules.iter().filter_map(|m| Some((m, place(m.kind, m.socket?)?))).collect();
    let cells = |p: &Placement| {
        (
            MARGIN + PITCH * p.col as f32,
            MARGIN + PITCH * p.row as f32,
            PITCH * p.cols as f32 - 9.0,
            PITCH * p.rows as f32 - 9.0,
        )
    };
    let mut rects = vec![turn_rect(base.0, base.1, base.2, base.3)];
    rects.extend(placed.iter().map(|(_, p)| {
        let (x, y, w, h) = cells(p);
        turn_rect(x, y, w, h)
    }));
    let x0 = rects.iter().map(|r| r.0).fold(f32::MAX, f32::min);
    let y0 = rects.iter().map(|r| r.1).fold(f32::MAX, f32::min);
    let width = rects.iter().map(|r| r.0 + r.2).fold(f32::MIN, f32::max) - x0;
    let height = rects.iter().map(|r| r.1 + r.3).fold(f32::MIN, f32::max) - y0;
    // a picture of natural size (w, h), centred on a turned rectangle and turned by `deg`
    let picture = |svg: String, rect: (f32, f32, f32, f32), (w, h): (f32, f32), deg: u16, title: &str, label: &str| {
        let (cx, cy) = ((rect.0 - x0 + rect.2 / 2.0) * SCALE, (rect.1 - y0 + rect.3 / 2.0) * SCALE);
        format!(
            "<figure class=mod title='{}' style='left:{cx:.1}px;top:{cy:.1}px'><div class=pic style='width:{:.1}px;height:{:.1}px;transform:translate(-50%,-50%) rotate({deg}deg)'>{svg}</div>{}</figure>",
            esc(title),
            w * SCALE,
            h * SCALE,
            if label.is_empty() { String::new() } else { format!("<figcaption>{}</figcaption>", esc(label)) }
        )
    };
    let mut out =
        format!("<div class=pictures style='width:{:.0}px;height:{:.0}px'>{DEFS}", width * SCALE, height * SCALE);
    out += &picture(base_picture(), rects[0], BASE_SIZE, base_turn % 360, "Base", "");
    // the contacts each module is plugged into (its USB port), over the pictures
    let mut ports = String::new();
    for (m, _) in &placed {
        if let Some((socket, (sx, sy))) = m.socket.and_then(|s| Some((s, contacts(s)?))) {
            let (x, y) = turn(MARGIN + PITCH * sx - 4.5, MARGIN + PITCH * sy - 4.5);
            ports += &format!(
                "<span class=port title='USB port (socket) {socket}' style='left:{:.1}px;top:{:.1}px'>{socket}</span>",
                (x - x0) * SCALE,
                (y - y0) * SCALE
            );
        }
    }
    for ((m, p), rect) in placed.iter().zip(&rects[1..]) {
        let up = (p.rotation + base_turn) % 360;
        let socket = m.socket.unwrap_or_default();
        out += &picture(
            module_picture(m.kind, up),
            *rect,
            picture_size(m.kind),
            up,
            &format!("{} · socket {socket} · {up}°", m.label),
            &format!("{} · {socket} · {up}°", m.label),
        );
    }
    out + &ports + "</div>"
}

/// The base picture's size: its 4×6 cells from 25 px, 129 px apart, and a 15 px edge.
const BASE_SIZE: (f32, f32) = (556.0, 814.0);

/// A module picture's size, unturned: its cells less the gap.
fn picture_size(kind: Kind) -> (f32, f32) {
    let (cols, rows) = footprint(kind);
    (129.0 * cols as f32 - 9.0, 129.0 * rows as f32 - 9.0)
}

/// A control on a module, unturned, in its picture's pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    Rect { x: f32, y: f32, w: f32, h: f32 },
    Circle { cx: f32, cy: f32, r: f32 },
}

/// Each control of a module in its own order (keys row by row, encoders as the module reports them), where
/// `module_picture` draws it: the dial's key (OpenDeck's, for its press) is in the strip under its screen.
fn control_shapes(kind: Kind) -> Vec<Shape> {
    let (w, h) = picture_size(kind);
    match kind {
        Kind::Keys => {
            let (size, gap, top) = (76.0, 14.0, 30.0);
            let left = (w - 5.0 * size - 4.0 * gap) / 2.0;
            (0..15)
                .map(|i| Shape::Rect {
                    x: left + (i % 5) as f32 * (size + gap),
                    y: top + (i / 5) as f32 * (size + gap),
                    w: size,
                    h: size,
                })
                .collect()
        }
        Kind::Knobs => [(0.26, 0.62), (0.74, 0.62), (0.5, 0.27)]
            .iter()
            .map(|&(x, y)| Shape::Circle { cx: w * x, cy: h * y, r: 40.0 })
            .collect(),
        Kind::Faders => {
            let travel = h - 38.0 - 66.0;
            (0..5)
                .map(|i| Shape::Rect {
                    x: 62.0 + i as f32 * (w - 124.0) / 4.0 - 20.0,
                    y: 16.0,
                    w: 40.0,
                    h: travel + 16.0,
                })
                .collect()
        }
        Kind::Rollers => {
            let wheel = (w - 60.0) / 2.0;
            // the module reports the right wheel first
            (0..2)
                .map(|i| Shape::Rect {
                    x: 20.0 + (1 - i) as f32 * (wheel + 20.0),
                    y: 14.0,
                    w: wheel,
                    h: h - 28.0 - 26.0,
                })
                .collect()
        }
        Kind::Dial => vec![Shape::Circle { cx: w / 2.0, cy: (h - 38.0) / 2.0 + 2.0, r: 84.0 }],
    }
}

/// The module's layout for OpenDeck (`layout` with `layoutVersion` 1 in `registerDevice`, nekename/OpenDeck#359;
/// OpenDeck without it ignores both): every key and encoder where it is on the module, turned `rot` degrees like
/// the module on the desk, by its OpenDeck position. OpenDeck hides positions a layout leaves out, so the dial's
/// key is there, and functions 2–4 of knobs and the dial get rows of small circles below the module.
pub fn layout(kind: Kind, rot: u16, functions: u8) -> serde_json::Value {
    let (w, h) = picture_size(kind);
    let rot = rot % 360;
    // a point of the unturned module, turned clockwise with it
    let turn = |x: f32, y: f32| match rot {
        90 => (h - y, x),
        180 => (w - x, h - y),
        270 => (y, w - x),
        _ => (x, y),
    };
    let (cw, ch) = if rot % 180 == 90 { (h, w) } else { (w, h) };
    let px = |v: f32| v.round().max(0.0) as u16;
    let encoders = crate::MODELS.iter().find(|m| m.kind == kind).map_or(0, |m| m.encoders);
    let rect = |controller: &str, position: u8, (x, y, rw, rh): (f32, f32, f32, f32), surface: Option<&str>| {
        let (a, b) = (turn(x, y), turn(x + rw, y + rh));
        let mut c = serde_json::json!({
            "controller": controller, "position": position, "shape": "rect",
            "x": px(a.0.min(b.0)), "y": px(a.1.min(b.1)), "width": px((a.0 - b.0).abs()), "height": px((a.1 - b.1).abs()),
        });
        if let Some(s) = surface {
            c["surface"] = s.into();
        }
        c
    };
    let circle = |controller: &str, position: u8, (cx, cy): (f32, f32), r: f32| serde_json::json!({ "controller": controller, "position": position, "shape": "circle", "cx": px(cx), "cy": px(cy), "r": px(r) });
    let mut controls = Vec::new();
    let mut surfaces = Vec::new();
    for (i, shape) in control_shapes(kind).into_iter().enumerate() {
        let (controller, position) = match kind {
            Kind::Keys => ("Keypad", crate::device::key_to_display(i as u8, rot)),
            _ => ("Encoder", crate::device::encoder_index(kind, rot, encoders, i as u8)),
        };
        controls.push(match shape {
            Shape::Rect { x, y, w, h } => {
                rect(controller, position, (x, y, w, h), (kind == Kind::Keys).then_some("screen"))
            }
            Shape::Circle { cx, cy, r } => circle(controller, position, turn(cx, cy), r),
        });
    }
    if kind == Kind::Keys {
        // one 480×272 screen under all the keys
        let keys = control_shapes(kind);
        let (Shape::Rect { x, y, .. }, Some(&Shape::Rect { x: x1, y: y1, w: kw, h: kh })) = (keys[0], keys.last())
        else {
            unreachable!()
        };
        let (a, b) = (turn(x, y), turn(x1 + kw, y1 + kh));
        let (sw, sh) = if rot % 180 == 90 { (272, 480) } else { (480, 272) };
        surfaces.push(serde_json::json!({
            "id": "screen", "kind": "display", "x": px(a.0.min(b.0)), "y": px(a.1.min(b.1)),
            "width": px((a.0 - b.0).abs()), "height": px((a.1 - b.1).abs()), "pixelWidth": sw, "pixelHeight": sh,
        }));
    }
    if kind == Kind::Dial {
        let (cx, cy) = turn(w / 2.0, (h - 38.0) / 2.0 + 2.0);
        surfaces.push(serde_json::json!({
            "id": "screen", "kind": "display", "x": px(cx - 62.0), "y": px(cy - 62.0), "width": 124, "height": 124,
            "pixelWidth": 240, "pixelHeight": 240,
        }));
        controls.push(circle("Keypad", 0, turn(w / 2.0, h - 19.0), 15.0));
    }
    // functions 2–4: a row each under the module, encoders in OpenDeck order
    const ROW: f32 = 56.0;
    let rows = if matches!(kind, Kind::Knobs | Kind::Dial) { functions.saturating_sub(1) } else { 0 };
    for f in 0..rows {
        for p in 0..encoders {
            let centre = (cw * (p as f32 + 0.5) / encoders as f32, ch + f as f32 * ROW + ROW / 2.0);
            controls.push(circle("Encoder", (f + 1) * encoders + p, centre, 18.0));
        }
    }
    serde_json::json!({
        "canvas": { "width": px(cw), "height": px(ch + rows as f32 * ROW), "unit": "px" },
        "surfaces": surfaces,
        "controls": controls,
    })
}

/// Gradients the pictures share, once per view.
const DEFS: &str = "<svg width=0 height=0 style='position:absolute' aria-hidden=true><defs>\
<linearGradient id=mh-body x1=0 y1=0 x2=0 y2=1><stop offset=0 stop-color='#2a2a31'/><stop offset=1 stop-color='#18181c'/></linearGradient>\
<linearGradient id=mh-metal x1=0 y1=0 x2=1 y2=1><stop offset=0 stop-color='#f4f4f8'/><stop offset=.45 stop-color='#5c5c66'/><stop offset=.7 stop-color='#d3d3db'/><stop offset=1 stop-color='#4a4a52'/></linearGradient>\
<radialGradient id=mh-cap cx=.4 cy=.35 r=.75><stop offset=0 stop-color='#45454e'/><stop offset=1 stop-color='#121215'/></radialGradient>\
<radialGradient id=mh-led r=.5><stop offset=.78 stop-color='#8f74ff' stop-opacity=.9/><stop offset=1 stop-color='#8f74ff' stop-opacity=0/></radialGradient>\
<radialGradient id=mh-screen cx=.4 cy=.3 r=.8><stop offset=0 stop-color='#34405a'/><stop offset=1 stop-color='#07080c'/></radialGradient>\
<linearGradient id=mh-key x1=0 y1=0 x2=0 y2=1><stop offset=0 stop-color='#31313a'/><stop offset=1 stop-color='#202026'/></linearGradient>\
<linearGradient id=mh-wheel x1=0 y1=0 x2=0 y2=1><stop offset=0 stop-color='#0d0d10'/><stop offset=.5 stop-color='#3a3a42'/><stop offset=1 stop-color='#0d0d10'/></linearGradient>\
</defs></svg>";

/// A module's picture, unturned, for a module turned `up` degrees: its controls numbered like their OpenDeck
/// positions (from 1), which the plugin renumbers for the turn (`key_to_display`, `encoder_index`), the numbers
/// turned back to stand upright. Unturned: keys row by row, knobs 1 bottom left, 2 bottom right, 3 at the top,
/// faders and rollers from the left (the rollers report the right wheel first).
fn module_picture(kind: Kind, up: u16) -> String {
    let (w, h) = picture_size(kind);
    let name = match kind {
        Kind::Keys => "15xKey",
        Kind::Knobs => "3xKnob",
        Kind::Faders => "5xFader",
        Kind::Rollers => "2xRoller",
        Kind::Dial => "Dial",
    };
    let strip = if kind == Kind::Rollers { 28.0 } else { 38.0 };
    // control `i` of the module (its own order) -> its OpenDeck position from 1, upright at (x, y)
    let encoders = crate::MODELS.iter().find(|m| m.kind == kind).map_or(1, |m| m.encoders);
    let shown = |i: usize| match kind {
        Kind::Keys => crate::device::key_to_display(i as u8, up) as usize + 1,
        _ => crate::device::encoder_index(kind, up, encoders, i as u8) as usize + 1,
    };
    let number = |x: f32, y: f32, i: usize| {
        format!("<text x='{x:.1}' y='{:.1}' font-size='13' fill='#8a8a96' text-anchor='middle' transform='rotate(-{up} {x:.1} {y:.1})'>{}</text>", y + 4.5, shown(i))
    };
    let mut parts = String::new();
    match kind {
        Kind::Keys => {
            let (size, gap, top) = (76.0, 14.0, 30.0);
            let left = (w - 5.0 * size - 4.0 * gap) / 2.0;
            for i in 0..15 {
                let (x, y) = (left + (i % 5) as f32 * (size + gap), top + (i / 5) as f32 * (size + gap));
                parts += &format!(
                    "<rect x='{x:.1}' y='{y:.1}' width='{size}' height='{size}' rx='9' fill='url(#mh-key)' stroke='#3c3c46'/>\
                     <rect x='{:.1}' y='{:.1}' width='{:.1}' height='{:.1}' rx='5' fill='#121216'/>{}",
                    x + 7.0,
                    y + 7.0,
                    size - 14.0,
                    size - 14.0,
                    number(x + size / 2.0, y + size / 2.0, i)
                );
            }
        }
        Kind::Knobs => {
            for (i, (cx, cy)) in [(0, (w * 0.26, h * 0.62)), (1, (w * 0.74, h * 0.62)), (2, (w * 0.5, h * 0.27))] {
                parts += &format!(
                    "<circle cx='{cx:.1}' cy='{cy:.1}' r='44' fill='url(#mh-led)'/>\
                     <circle cx='{cx:.1}' cy='{cy:.1}' r='37' fill='url(#mh-metal)'/>\
                     <circle cx='{cx:.1}' cy='{cy:.1}' r='32' fill='url(#mh-cap)'/>\
                     <line x1='{cx:.1}' y1='{:.1}' x2='{cx:.1}' y2='{:.1}' stroke='#e2e2ea' stroke-width='3' stroke-linecap='round'/>{}",
                    cy - 27.0,
                    cy - 16.0,
                    number(cx, cy + 5.0, i)
                );
            }
        }
        Kind::Faders => {
            let caps = [0.6, 0.3, 0.75, 0.15, 0.45];
            for (i, at) in caps.iter().enumerate() {
                let x = 62.0 + i as f32 * (w - 124.0) / 4.0;
                let (top, travel) = (24.0, h - strip - 66.0);
                parts += &format!(
                    "<rect x='{:.1}' y='{top}' width='8' height='{travel:.1}' rx='4' fill='#09090c' stroke='#2e2e36'/>\
                     <rect x='{:.1}' y='{:.1}' width='38' height='16' rx='4' fill='url(#mh-metal)'/>{}",
                    x - 4.0,
                    x - 19.0,
                    top + at * travel - 8.0,
                    number(x, top + travel + 18.0, i)
                );
            }
        }
        Kind::Rollers => {
            let wheel = (w - 60.0) / 2.0;
            for i in 0..2 {
                let x = 20.0 + i as f32 * (wheel + 20.0);
                parts += &format!("<rect x='{x:.1}' y='14' width='{wheel:.1}' height='{:.1}' rx='10' fill='url(#mh-wheel)' stroke='#2e2e36'/>", h - strip - 26.0);
                for r in 0..7 {
                    let y = 20.0 + r as f32 * (h - strip - 38.0) / 6.0;
                    parts += &format!(
                        "<line x1='{:.1}' y1='{y:.1}' x2='{:.1}' y2='{y:.1}' stroke='#000' stroke-opacity='.35'/>",
                        x + 6.0,
                        x + wheel - 6.0
                    );
                }
                parts += &number(x + wheel / 2.0, h - strip - 5.0, 1 - i as usize);
            }
        }
        Kind::Dial => {
            let (cx, cy) = (w / 2.0, (h - strip) / 2.0 + 2.0);
            parts += &format!(
                "<circle cx='{cx:.1}' cy='{cy:.1}' r='92' fill='url(#mh-led)'/>\
                 <circle cx='{cx:.1}' cy='{cy:.1}' r='84' fill='url(#mh-metal)'/>\
                 <circle cx='{cx:.1}' cy='{cy:.1}' r='76' fill='#0b0b0e'/>\
                 <circle cx='{cx:.1}' cy='{cy:.1}' r='62' fill='url(#mh-screen)'/>\
                 <path d='M {:.1} {:.1} A 50 50 0 0 1 {:.1} {:.1}' fill='none' stroke='#fff' stroke-opacity='.18' stroke-width='5' stroke-linecap='round'/>",
                cx - 38.0,
                cy - 22.0,
                cx + 8.0,
                cy - 50.0
            );
        }
    }
    format!(
        "<svg viewBox='0 0 {w} {h}' xmlns='http://www.w3.org/2000/svg' font-family='system-ui,sans-serif'>\
         <rect width='{w}' height='{h}' rx='14' fill='url(#mh-body)'/>\
         <path d='M 0 {:.1} H {w} V {:.1} Q {w} {h} {:.1} {h} H 14 Q 0 {h} 0 {:.1} Z' fill='#2c2c33'/>{parts}\
         <text x='{:.1}' y='{:.1}' font-size='15' font-weight='600' fill='#a4a4b0' text-anchor='middle'>{name}</text></svg>",
        h - strip,
        h - 14.0,
        w - 14.0,
        h - 14.0,
        w / 2.0,
        h - strip / 2.0 + 5.0
    )
}

/// The base, unturned: every socket's contacts (`contacts`), with its number.
fn base_picture() -> String {
    let (w, h) = BASE_SIZE;
    let mut parts = String::new();
    for c in 0..=COLS {
        let x = 25.0 + 129.0 * c as f32 - 4.5;
        parts += &format!("<line x1='{x:.1}' y1='20' x2='{x:.1}' y2='{:.1}' stroke='#1f1f25'/>", h - 20.0);
    }
    for r in 0..=ROWS {
        let y = 25.0 + 129.0 * r as f32 - 4.5;
        parts += &format!("<line x1='20' y1='{y:.1}' x2='{:.1}' y2='{y:.1}' stroke='#1f1f25'/>", w - 20.0);
    }
    for &(socket, ..) in SOCKETS {
        let (px, py) = contacts(socket).unwrap();
        let (x, y) = (25.0 + 129.0 * px - 4.5, 25.0 + 129.0 * py - 4.5);
        for (dx, dy) in [(-5.0, -5.0), (5.0, -5.0), (-5.0, 5.0), (5.0, 5.0)] {
            parts += &format!(
                "<circle cx='{:.1}' cy='{:.1}' r='3.4' fill='#3a3a44' stroke='#55555f' stroke-width='.8'/>",
                x + dx,
                y + dy
            );
        }
        parts += &format!("<text x='{:.1}' y='{:.1}' font-size='12' fill='#4f4f5a'>{socket}</text>", x + 11.0, y - 9.0);
    }
    format!(
        "<svg viewBox='0 0 {w} {h}' xmlns='http://www.w3.org/2000/svg' font-family='system-ui,sans-serif'>\
         <rect width='{w}' height='{h}' rx='16' fill='#111114' stroke='#24242b'/>{parts}</svg>"
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(kind: Kind, s: &str) -> (i8, i8, u16) {
        let p = place(kind, s).unwrap();
        (p.col, p.row, p.rotation)
    }

    /// Places modules, checks nothing overlaps and returns the base cells left bare.
    fn bare(mods: &[(Kind, &str)]) -> Vec<(i8, i8)> {
        let mut covered = std::collections::HashSet::new();
        for &(kind, s) in mods {
            let p = place(kind, s).unwrap();
            for r in p.row..p.row + p.rows {
                for c in p.col..p.col + p.cols {
                    assert!(covered.insert((c, r)), "{kind:?} overlaps at {c},{r}");
                }
            }
        }
        (0..ROWS)
            .flat_map(|r| (0..COLS).map(move |c| (c, r)))
            .filter(|cell| !covered.contains(&(cell.0, cell.1)))
            .collect()
    }

    #[test]
    fn every_socket_places_every_module() {
        for &(s, ..) in SOCKETS {
            for kind in [Kind::Keys, Kind::Faders, Kind::Knobs, Kind::Dial, Kind::Rollers] {
                let p = place(kind, s).unwrap_or_else(|| panic!("{kind:?} at {s}"));
                let (w, h) = footprint(kind);
                assert_eq!((p.cols, p.rows), if p.rotation % 180 == 90 { (h, w) } else { (w, h) });
            }
        }
        assert!(place(Kind::Keys, "99").is_none());
    }

    // Entries of the official app's tables (docs/masterhub-app-analysis.md), one row of sockets per module.
    #[test]
    fn matches_the_official_app() {
        for (s, keys, faders, knobs, rollers) in [
            ("22", (0, 2, 270), (1, 2, 270), (1, 2, 270), None),
            ("23", (1, 2, 270), (2, 2, 270), (2, 2, 270), Some((0, 2, 0))),
            ("33", (0, 2, 180), (0, 2, 180), (0, 2, 180), Some((0, 2, 270))),
            ("41", (0, 2, 0), (0, 3, 0), (2, 3, 0), Some((3, 1, 90))),
            ("134", (1, 0, 270), (2, 0, 270), (2, 0, 270), Some((0, 0, 0))),
        ] {
            assert_eq!(at(Kind::Keys, s), keys, "15xKey at {s}");
            assert_eq!(at(Kind::Faders, s), faders, "5xFader at {s}");
            assert_eq!(at(Kind::Knobs, s), knobs, "3xKnob at {s}");
            assert_eq!(at(Kind::Dial, s), knobs, "dial at {s}");
            if let Some(r) = rollers {
                assert_eq!(at(Kind::Rollers, s), r, "2xRoller at {s}");
            }
        }
        // 121 from the rollers' entry: the app's dial entry for it is wrong
        assert_eq!(at(Kind::Rollers, "121"), (0, 5, 0));
        assert_eq!(at(Kind::Dial, "121"), (2, 5, 270));
        // every socket's contacts in a place of their own
        let spots: std::collections::HashSet<String> =
            SOCKETS.iter().map(|s| format!("{:?}", contacts(s.0).unwrap())).collect();
        assert_eq!(spots.len(), SOCKETS.len());
    }

    // This unit (2026-10-25): 15xKey 23, 2xRoller 33, dial 134, 3xKnob 143 fill the base; the 15xKey stands
    // upright on a flat (landscape) base.
    #[test]
    fn this_unit() {
        assert!(
            bare(&[(Kind::Keys, "23"), (Kind::Rollers, "33"), (Kind::Dial, "134"), (Kind::Knobs, "143")]).is_empty()
        );
        assert_eq!(at(Kind::Keys, "23"), (1, 2, 270));
    }

    /// Writes target/placement-*.svg to look at: `cargo test dump_placement -- --ignored`
    #[test]
    #[ignore]
    fn dump_placement() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        let mine = [Shown { kind: Kind::Keys, label: "15xKey", socket: Some("23") }];
        std::fs::write(dir.join("placement-mine.svg"), svg(&mine, 90)).unwrap();
        let full = [
            Shown { kind: Kind::Keys, label: "15xKey", socket: Some("23") },
            Shown { kind: Kind::Rollers, label: "2xRoller", socket: Some("33") },
            Shown { kind: Kind::Faders, label: "5xFader", socket: Some("113") },
        ];
        std::fs::write(dir.join("placement-full.svg"), svg(&full, 0)).unwrap();
        // the pictures view of it, as a page of its own
        let css = "body{margin:0;background:#16161b;display:flex;justify-content:center;padding:24px}.pictures{position:relative}\
                   .pictures .port{position:absolute;transform:translate(-50%,-50%);min-width:22px;height:22px;padding:0 5px;border-radius:999px;\
                   background:#fff;color:#16161b;font:700 11px/22px system-ui;text-align:center;box-shadow:0 0 0 2px #16161b}\
                   .pictures figure{position:absolute;margin:0}.pictures .pic{position:absolute;left:0;top:0;filter:drop-shadow(0 4px 10px rgba(0,0,0,.45))}\
                   .pictures .pic svg{display:block;width:100%;height:100%}.pictures figcaption{position:absolute;transform:translate(-50%,-50%);\
                   background:rgba(0,0,0,.65);color:#fff;font:11px system-ui;padding:2px 7px;border-radius:999px;white-space:nowrap}";
        std::fs::write(
            dir.join("placement-pictures-full.html"),
            format!("<!doctype html><meta charset=utf-8><style>{css}</style>{}", pictures(&full, 0)),
        )
        .unwrap();
    }

    #[test]
    fn pictures_view() {
        let html = pictures(
            &[
                Shown { kind: Kind::Keys, label: "15xKey", socket: Some("22") },
                Shown { kind: Kind::Dial, label: "Dial", socket: Some("134") },
            ],
            90,
        );
        assert_eq!(html.matches("<svg viewBox").count(), 3); // the base and both modules, the plugin's own pictures
        assert!(html.contains("class=port title='USB port (socket) 22'")); // the port each module uses, marked
        assert!(html.contains(">15xKey</text>") && html.contains(">Dial</text>") && !html.contains("<img"));
        assert!(html.contains("15xKey · 22 · 0°")); // 270° on the base + 90°
        assert!(html.contains("rotate(90deg)")); // the base drawing turned with the desk
    }

    /// The numbers a module picture shows, in the module's own control order.
    fn numbers(svg: &str) -> Vec<u32> {
        svg.split("</text>").filter_map(|t| t.rsplit('>').next()?.parse().ok()).collect()
    }

    #[test]
    fn pictures_number_controls_like_opendeck() {
        // unturned: knobs 1 bottom left, 2 bottom right, 3 top; rollers' left wheel 1; keys row by row
        assert_eq!(numbers(&module_picture(Kind::Knobs, 0)), [1, 2, 3]);
        assert_eq!(numbers(&module_picture(Kind::Rollers, 0)), [1, 2]);
        assert_eq!(numbers(&module_picture(Kind::Keys, 0)), (1..=15).collect::<Vec<_>>());
        // upside down the first control is last, as the plugin maps it; numbers turned back upright
        let turned = module_picture(Kind::Knobs, 180);
        assert_eq!(numbers(&turned), [3, 2, 1]);
        assert!(turned.contains("rotate(-180 "));
        // a 15xKey turned 90°: the key at its top left is in OpenDeck's top right (3 columns × 5 rows)
        assert_eq!(numbers(&module_picture(Kind::Keys, 90))[0], 3);
    }

    /// Every key and encoder position OpenDeck has for the module is in its layout, once, inside the canvas, and
    /// the layout turns with the module.
    #[test]
    fn layouts_cover_every_position() {
        if let Ok(dir) = std::env::var("MASTERHUB_LAYOUTS") {
            // the layouts as JSON, to look at: MASTERHUB_LAYOUTS=<dir> cargo test layouts
            for model in crate::MODELS {
                for rot in [0, 90] {
                    let file = format!("{dir}/{:?}-{rot}.json", model.kind);
                    std::fs::write(file, layout(model.kind, rot, 2).to_string()).unwrap();
                }
            }
        }
        for model in crate::MODELS {
            for rot in [0, 90, 180, 270] {
                for functions in 1..=4u8 {
                    let l = layout(model.kind, rot, functions);
                    let (cw, ch) = (l["canvas"]["width"].as_u64().unwrap(), l["canvas"]["height"].as_u64().unwrap());
                    let mut seen = std::collections::HashSet::new();
                    for c in l["controls"].as_array().unwrap() {
                        assert!(seen.insert((c["controller"].to_string(), c["position"].as_u64().unwrap())), "{c}");
                        let (x0, y0, x1, y1) = match c["shape"].as_str().unwrap() {
                            "rect" => {
                                let (x, y) = (c["x"].as_u64().unwrap(), c["y"].as_u64().unwrap());
                                (x, y, x + c["width"].as_u64().unwrap(), y + c["height"].as_u64().unwrap())
                            }
                            _ => {
                                let (x, y, r) =
                                    (c["cx"].as_u64().unwrap(), c["cy"].as_u64().unwrap(), c["r"].as_u64().unwrap());
                                (x - r, y - r, x + r, y + r)
                            }
                        };
                        assert!(x1 <= cw && y1 <= ch && x0 < x1 && y0 < y1, "{:?} {rot}° {c}", model.kind);
                    }
                    let functions = if matches!(model.kind, Kind::Knobs | Kind::Dial) { functions } else { 1 };
                    let keys = if model.kind == Kind::Dial { 1 } else { (model.rows * model.columns) as usize };
                    assert_eq!(seen.len(), keys + (model.encoders * functions) as usize, "{:?} {rot}°", model.kind);
                }
            }
        }
        // turned by 90° the 15xKey shows 3 columns: OpenDeck's position 2 (the module's top-left key) right of 0
        let (flat, turned) = (layout(Kind::Keys, 0, 1), layout(Kind::Keys, 90, 1));
        assert_eq!(
            (flat["canvas"]["width"].clone(), flat["canvas"]["height"].clone()),
            (turned["canvas"]["height"].clone(), turned["canvas"]["width"].clone())
        );
        let key = |l: &serde_json::Value, p: u64| {
            l["controls"].as_array().unwrap().iter().find(|c| c["position"] == p).unwrap().clone()
        };
        assert_eq!(key(&flat, 0)["x"], 36);
        assert!(key(&turned, 2)["x"].as_u64().unwrap() > key(&turned, 0)["x"].as_u64().unwrap());
        // the knobs' triangle: the top knob (position 3) above the other two
        let knobs = layout(Kind::Knobs, 0, 1);
        assert!(key(&knobs, 2)["cy"].as_u64().unwrap() < key(&knobs, 0)["cy"].as_u64().unwrap());
    }

    #[test]
    fn draws_the_modules() {
        let svg = svg(
            &[
                Shown { kind: Kind::Keys, label: "15xKey", socket: Some("23") },
                Shown { kind: Kind::Dial, label: "Dial", socket: None },
            ],
            90,
        );
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains("socket 23 · 0°")); // 270° on the base + 90° base turn
        assert_eq!(svg.matches("<g class=port>").count(), 1); // the 15xKey's port marked; the dial has none
        assert_eq!(svg.matches("rx='6'").count(), 1); // the dial without a socket isn't drawn
    }
}
