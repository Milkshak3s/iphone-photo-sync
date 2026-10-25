//! The app icon, drawn in code so there are no binary assets: a blue rounded
//! square with a white sun and mountains, i.e. "photos".

pub const SIZE: u32 = 32;

pub fn rgba() -> Vec<u8> {
    let s = SIZE as f32;
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let bg = rounded_rect_coverage(fx, fy, s, 7.0);
            let sun = circle_coverage(fx, fy, s * 0.68, s * 0.32, s * 0.11);
            let hills = mountain(fx / s, fy / s);
            let white = sun.max(hills);
            // Blue gradient from top to bottom.
            let t = fy / s;
            let (r, g, b) = (
                lerp(52.0, 22.0, t),
                lerp(152.0, 101.0, t),
                lerp(255.0, 216.0, t),
            );
            let mix = |c: f32| (c * (1.0 - white) + 255.0 * white) as u8;
            px.extend_from_slice(&[mix(r), mix(g), mix(b), (bg * 255.0) as u8]);
        }
    }
    px
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn rounded_rect_coverage(x: f32, y: f32, size: f32, radius: f32) -> f32 {
    let half = size / 2.0;
    let qx = (x - half).abs() - (half - radius - 1.0);
    let qy = (y - half).abs() - (half - radius - 1.0);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0);
    (radius - outside + 0.5).clamp(0.0, 1.0)
}

fn circle_coverage(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> f32 {
    let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    (r - d + 0.5).clamp(0.0, 1.0)
}

/// Two overlapping peaks in unit coordinates, cut off above the bottom margin.
fn mountain(u: f32, v: f32) -> f32 {
    if !(0.16..=0.84).contains(&u) || v > 0.80 {
        return 0.0;
    }
    let peak1 = 0.38 + (u - 0.38).abs() * 1.4;
    let peak2 = 0.50 + (u - 0.64).abs() * 1.2;
    let top = peak1.min(peak2);
    ((v - top) * 32.0 + 0.5).clamp(0.0, 1.0)
}
