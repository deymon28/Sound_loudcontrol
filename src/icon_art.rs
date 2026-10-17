// Procedural app icon, shared by the runtime (window/tray) and build.rs (.exe icon).
// Dependency-free on purpose so build.rs can `include!` it.

/// Icon flavour: normal, currently ducking, paused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum IconState {
    Normal,
    Ducking,
    Paused,
}

/// Renders a `size`×`size` RGBA icon: rounded tile with level bars, the right pair lowered.
pub fn render_icon(size: u32, state: IconState) -> Vec<u8> {
    let accent: [f32; 3] = match state {
        IconState::Normal => [0.30, 0.86, 0.78],
        IconState::Ducking => [1.00, 0.70, 0.28],
        IconState::Paused => [0.55, 0.58, 0.64],
    };
    let dim: [f32; 3] = [accent[0] * 0.55, accent[1] * 0.55, accent[2] * 0.55];
    let bg: [f32; 3] = [0.09, 0.10, 0.13];
    // Bars as (center x, height) in unit coordinates; the last two are "ducked".
    let bars: [(f32, f32, bool); 4] = [(0.29, 0.50, false), (0.43, 0.66, false), (0.57, 0.30, true), (0.71, 0.22, true)];
    let bar_w = 0.10;
    let base_y = 0.76;

    let n = size as usize;
    let mut out = vec![0u8; n * n * 4];
    const SS: usize = 4;
    for py in 0..n {
        for px in 0..n {
            let mut acc = [0.0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let mut color: Option<[f32; 3]> = None;
                    if in_round_rect(x, y, 0.04, 0.04, 0.96, 0.96, 0.22) {
                        color = Some(bg);
                        for &(cx, h, ducked) in &bars {
                            if in_round_rect(x, y, cx - bar_w / 2.0, base_y - h, cx + bar_w / 2.0, base_y, bar_w / 2.0) {
                                color = Some(if ducked { dim } else { accent });
                            }
                        }
                        // Thin accent baseline under the bars.
                        if in_round_rect(x, y, 0.20, base_y + 0.06, 0.80, base_y + 0.10, 0.02) {
                            color = Some(accent);
                        }
                    }
                    if let Some(c) = color {
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        acc[3] += 1.0;
                    }
                }
            }
            let i = (py * n + px) * 4;
            if acc[3] > 0.0 {
                let a = acc[3] / (SS * SS) as f32;
                out[i] = (acc[0] / acc[3] * 255.0).round() as u8;
                out[i + 1] = (acc[1] / acc[3] * 255.0).round() as u8;
                out[i + 2] = (acc[2] / acc[3] * 255.0).round() as u8;
                out[i + 3] = (a * 255.0).round() as u8;
            }
        }
    }
    out
}

fn in_round_rect(x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if x < x0 || x > x1 || y < y0 || y > y1 {
        return false;
    }
    let cx = x.clamp(x0 + r, x1 - r);
    let cy = y.clamp(y0 + r, y1 - r);
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}
