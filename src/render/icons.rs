//! chrome icon set, drawn as strokes on a 10-unit grid and rasterized to
//! anti-aliased coverage at the exact device size, so every icon shares one
//! stroke weight and lands crisp at any scale

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Icon {
    Close,
    Minimize,
    Maximize,
    Restore,
    Plus,
    Chevron,
    ChevronUp,
    SplitV,
    SplitH,
    Grid,
    Gear,
    Search,
    Bell,
    Check,
    Shield,
    Arrow,
}

enum Prim {
    Line(f32, f32, f32, f32),
    /// rounded-rect outline: x, y, w, h, corner radius
    Frame(f32, f32, f32, f32, f32),
    /// filled rounded rect at reduced opacity: x, y, w, h, alpha
    Tint(f32, f32, f32, f32, f32),
    Ring(f32, f32, f32),
    /// arc of a circle: cx, cy, r, start and end angle in radians (y down)
    Arc(f32, f32, f32, f32, f32),
}

fn prims(icon: Icon) -> Vec<Prim> {
    use std::f32::consts::PI;
    use Prim::*;
    match icon {
        Icon::Close => vec![Line(1.5, 1.5, 8.5, 8.5), Line(8.5, 1.5, 1.5, 8.5)],
        Icon::Minimize => vec![Line(1.0, 5.0, 9.0, 5.0)],
        Icon::Maximize => vec![Frame(1.0, 1.0, 8.0, 8.0, 1.0)],
        Icon::Restore => vec![
            Frame(1.0, 3.0, 6.0, 6.0, 1.0),
            Line(3.0, 1.0, 8.0, 1.0),
            Line(9.0, 2.0, 9.0, 7.0),
            Arc(8.0, 2.0, 1.0, -PI / 2.0, 0.0),
        ],
        Icon::Plus => vec![Line(5.0, 1.0, 5.0, 9.0), Line(1.0, 5.0, 9.0, 5.0)],
        Icon::Chevron => vec![Line(2.0, 3.75, 5.0, 6.75), Line(5.0, 6.75, 8.0, 3.75)],
        Icon::ChevronUp => vec![Line(2.0, 6.25, 5.0, 3.25), Line(5.0, 3.25, 8.0, 6.25)],
        Icon::SplitV => vec![Frame(0.5, 1.5, 9.0, 7.0, 1.5), Line(5.0, 1.5, 5.0, 8.5), Tint(0.5, 1.5, 4.5, 7.0, 0.35)],
        Icon::SplitH => vec![Frame(0.5, 1.5, 9.0, 7.0, 1.5), Line(0.5, 5.0, 9.5, 5.0), Tint(0.5, 1.5, 9.0, 3.5, 0.35)],
        Icon::Grid => vec![
            Frame(1.0, 1.0, 3.25, 3.25, 0.75),
            Frame(5.75, 1.0, 3.25, 3.25, 0.75),
            Frame(1.0, 5.75, 3.25, 3.25, 0.75),
            Frame(5.75, 5.75, 3.25, 3.25, 0.75),
        ],
        // settings as two sliders: lines broken by their knobs
        Icon::Gear => vec![
            Line(0.75, 3.0, 2.0, 3.0),
            Ring(3.5, 3.0, 1.4),
            Line(5.0, 3.0, 9.25, 3.0),
            Line(0.75, 7.0, 5.0, 7.0),
            Ring(6.5, 7.0, 1.4),
            Line(8.0, 7.0, 9.25, 7.0),
        ],
        Icon::Search => vec![Ring(4.25, 4.25, 3.0), Line(6.5, 6.5, 9.0, 9.0)],
        Icon::Bell => vec![
            Arc(5.0, 4.5, 2.75, PI, 2.0 * PI),
            Line(2.25, 4.5, 2.25, 7.0),
            Line(7.75, 4.5, 7.75, 7.0),
            Line(1.0, 7.5, 9.0, 7.5),
            Line(4.0, 9.0, 6.0, 9.0),
        ],
        Icon::Check => vec![Line(1.5, 5.25, 4.0, 7.75), Line(4.0, 7.75, 8.75, 2.5)],
        Icon::Shield => vec![
            Line(5.0, 0.75, 9.0, 2.25),
            Line(5.0, 0.75, 1.0, 2.25),
            Line(1.0, 2.25, 1.0, 5.0),
            Line(9.0, 2.25, 9.0, 5.0),
            Arc(5.0, 5.0, 4.0, 0.0, PI / 2.0),
            Arc(5.0, 5.0, 4.0, PI / 2.0, PI),
        ],
        Icon::Arrow => vec![Line(5.0, 9.0, 5.0, 1.5), Line(1.75, 4.75, 5.0, 1.5), Line(5.0, 1.5, 8.25, 4.75)],
    }
}

fn seg_dist(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    ((px - x0 - t * dx).powi(2) + (py - y0 - t * dy).powi(2)).sqrt()
}

/// signed distance to a rounded rect (negative inside)
fn round_rect_sd(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let qx = (px - cx).abs() - (w / 2.0 - r);
    let qy = (py - cy).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

fn arc_dist(px: f32, py: f32, cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> f32 {
    let mut a = (py - cy).atan2(px - cx);
    while a < a0 {
        a += std::f32::consts::TAU;
    }
    if a <= a1 {
        return (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r).abs();
    }
    let end = |t: f32| {
        let (s, c) = t.sin_cos();
        ((px - cx - c * r).powi(2) + (py - cy - s * r).powi(2)).sqrt()
    };
    end(a0).min(end(a1))
}

/// coverage bitmap (row-major, `size` x `size`) for `icon` with strokes
/// `stroke` device pixels wide
pub fn rasterize(icon: Icon, size: u32, stroke: f32) -> Vec<u8> {
    let unit = size as f32 / 10.0;
    let half = stroke / 2.0;
    let shapes = prims(icon);
    let mut out = vec![0u8; (size * size) as usize];
    for row in 0..size {
        for col in 0..size {
            let (px, py) = ((col as f32 + 0.5) / unit, (row as f32 + 0.5) / unit);
            let mut cov: f32 = 0.0;
            for p in &shapes {
                let c = match *p {
                    Prim::Line(x0, y0, x1, y1) => half + 0.5 - seg_dist(px, py, x0, y0, x1, y1) * unit,
                    Prim::Frame(x, y, w, h, r) => half + 0.5 - round_rect_sd(px, py, x, y, w, h, r).abs() * unit,
                    Prim::Tint(x, y, w, h, a) => (0.5 - round_rect_sd(px, py, x, y, w, h, 0.0) * unit).clamp(0.0, 1.0) * a,
                    Prim::Ring(cx, cy, r) => {
                        half + 0.5 - (((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r).abs() * unit
                    }
                    Prim::Arc(cx, cy, r, a0, a1) => half + 0.5 - arc_dist(px, py, cx, cy, r, a0, a1) * unit,
                };
                cov = cov.max(c.clamp(0.0, 1.0));
            }
            out[(row * size + col) as usize] = (cov * 255.0).round() as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 16] = [
        Icon::Close,
        Icon::Minimize,
        Icon::Maximize,
        Icon::Restore,
        Icon::Plus,
        Icon::Chevron,
        Icon::ChevronUp,
        Icon::SplitV,
        Icon::SplitH,
        Icon::Grid,
        Icon::Gear,
        Icon::Search,
        Icon::Bell,
        Icon::Check,
        Icon::Shield,
        Icon::Arrow,
    ];

    #[test]
    fn every_icon_paints_inside_its_box() {
        for icon in ALL {
            for size in [10, 15, 20, 30] {
                let px = rasterize(icon, size, (size as f32 / 10.0).max(1.0));
                let ink: u32 = px.iter().map(|&v| v as u32).sum();
                assert!(ink > 255 * size / 2, "{icon:?} at {size}px is nearly empty");
                // a thin diagonal never fully covers a pixel center, but every
                // icon must have strokes dense enough to read as crisp
                let peak = px.iter().copied().max().unwrap_or(0);
                assert!(peak >= 190 || size < 15, "{icon:?} at {size}px peaks at {peak}");
            }
        }
    }

    #[test]
    fn close_is_symmetric() {
        let px = rasterize(Icon::Close, 20, 2.0);
        for r in 0..20 {
            for c in 0..20 {
                assert_eq!(px[r * 20 + c], px[r * 20 + (19 - c)]);
            }
        }
    }
}
