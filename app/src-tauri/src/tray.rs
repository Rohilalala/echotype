//! Menu-bar mode (macOS): the status item is the live indicator instead of the pill. Its glyphs
//! are drawn here as monochrome template images, so macOS tints them for light and dark menu
//! bars, and change ~8 times a second only while listening or working.

use crate::settings::Settings;
use crate::{lock, state};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri::image::Image;

/// What the status item shows, from the same model that decides what the pill shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Glyph {
    Idle,
    /// Listening: the live input level.
    Wave,
    /// Transcribing or cleaning up.
    Working,
    /// Pasted or copied, for as long as the pill's notice would stay.
    Done,
    /// An error or a microphone problem.
    Warning,
}

/// Status items are 18 pt tall; drawn at 2x.
const PX: usize = 36;
/// Per-bar weights, so a level reads as a waveform rather than a block.
const WEIGHT: [f32; 5] = [0.55, 0.85, 1.0, 0.8, 0.5];
const BAR_X: [f64; 5] = [8.0, 13.0, 18.0, 23.0, 28.0];

fn dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy).max(1e-9)).clamp(0.0, 1.0);
    (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy)
}

/// A rounded vertical bar `h` px tall, centred on the glyph's middle line.
fn bar(p: (f64, f64), x: f64, h: f64) -> bool {
    let half = (h / 2.0 - 1.5).max(0.0);
    dist(p, (x, 18.0 - half), (x, 18.0 + half)) <= 1.5
}

fn inside_triangle(p: (f64, f64), [a, b, c]: [(f64, f64); 3]) -> bool {
    let side = |u: (f64, f64), v: (f64, f64)| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

/// Tiny deterministic jitter in 0..1 per frame and bar, so a steady voice still looks alive.
fn jitter(frame: u64, i: usize) -> f32 {
    let mut x = frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (i as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    (x % 1000) as f32 / 1000.0
}

/// RGBA pixels (black, coverage in alpha) for `glyph` at animation `frame`.
pub fn draw(glyph: Glyph, frame: u64, meter: f32) -> Vec<u8> {
    let shape: Box<dyn Fn((f64, f64)) -> f64> = match glyph {
        // A still waveform: Echotype at rest.
        Glyph::Idle => Box::new(|p| {
            let h = [10.0, 18.0, 26.0, 16.0, 8.0];
            f64::from((0..5).any(|i| bar(p, BAR_X[i], h[i])))
        }),
        Glyph::Wave => {
            let h: Vec<f64> = (0..5).map(|i| f64::from(6.0 + 22.0 * (meter * WEIGHT[i] * (0.75 + 0.5 * jitter(frame, i))).clamp(0.0, 1.0))).collect();
            Box::new(move |p| f64::from((0..5).any(|i| bar(p, BAR_X[i], h[i]))))
        }
        // Three dots, one lit in turn.
        Glyph::Working => Box::new(move |p| {
            (0..3).map(|i| if (p.0 - (10.0 + 8.0 * i as f64)).hypot(p.1 - 18.0) <= 3.0 { if frame % 3 == i { 1.0 } else { 0.35 } } else { 0.0 }).fold(0.0, f64::max)
        }),
        Glyph::Done => Box::new(|p| f64::from(dist(p, (8.0, 19.0), (15.0, 26.0)).min(dist(p, (15.0, 26.0), (28.0, 10.0))) <= 2.2)),
        // A triangle with the exclamation mark cut out.
        Glyph::Warning => Box::new(|p| {
            let mark = dist(p, (18.0, 13.0), (18.0, 21.0)) <= 1.8 || (p.0 - 18.0).hypot(p.1 - 26.5) <= 2.0;
            f64::from(inside_triangle(p, [(18.0, 4.0), (33.0, 31.0), (3.0, 31.0)]) && !mark)
        }),
    };
    let mut px = vec![0u8; PX * PX * 4];
    for y in 0..PX {
        for x in 0..PX {
            // 4x4 samples per pixel for smooth edges.
            let mut a = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    a += shape((x as f64 + (sx as f64 + 0.5) / 4.0, y as f64 + (sy as f64 + 0.5) / 4.0));
                }
            }
            px[(y * PX + x) * 4 + 3] = (a / 16.0 * 255.0).round() as u8;
        }
    }
    px
}

/// What the status item was last given, so the loop only touches it on a change.
#[derive(Default)]
pub struct Tray {
    mode: Option<bool>,
    icon: Option<Vec<u8>>,
    menu: Option<String>,
    mics: Option<(Instant, Vec<String>)>,
}

impl Tray {
    /// Applies menu-bar mode (or undoes it) and, in it, the glyph and the menu.
    pub fn sync(&mut self, app: &AppHandle, s: &Settings, bar: bool, glyph: Glyph, frame: u64, meter: f32, line: Option<&str>) {
        let Some(tray) = app.tray_by_id("main") else { return };
        if self.mode != Some(bar) {
            // Left click dictates in the menu bar; the menu moves to right click.
            let _ = tray.set_show_menu_on_left_click(!bar);
            if !bar && self.mode.is_some() {
                let _ = tray.set_icon_with_as_template(app.default_window_icon().cloned(), false);
                let _ = tray.set_menu(lock(&state(app).tray_menu).clone());
            }
            *self = Tray { mode: Some(bar), ..Tray::default() };
        }
        if !bar {
            return;
        }
        let px = draw(glyph, frame, meter);
        if self.icon.as_ref() != Some(&px) {
            let _ = tray.set_icon_with_as_template(Some(Image::new_owned(px.clone(), PX as u32, PX as u32)), true);
            self.icon = Some(px);
        }
        // ponytail: the microphone list is re-read every 5 s; a CoreAudio device listener if that ever lags.
        if self.mics.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(5)) {
            self.mics = Some((Instant::now(), crate::audio::input_devices().into_iter().map(|d| d.name).collect()));
        }
        let mics = &self.mics.as_ref().expect("just filled").1;
        let recording = state(app).recording.load(std::sync::atomic::Ordering::SeqCst);
        let has_last = lock(&state(app).last_output).is_some();
        let key = format!("{line:?}|{recording}|{has_last}|{:?}|{}|{mics:?}", s.input_device, s.language);
        if self.menu.as_ref() != Some(&key)
            && let Ok(m) = crate::pill::menu(app, mics, Some(crate::pill::Bar { line, recording }))
        {
            let _ = tray.set_menu(Some(m));
            self.menu = Some(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(px: &[u8]) -> Vec<u8> {
        px.chunks(4).map(|p| p[3]).collect()
    }

    #[test]
    fn glyphs_are_template_images_and_each_state_looks_different() {
        let all = [Glyph::Idle, Glyph::Wave, Glyph::Working, Glyph::Done, Glyph::Warning];
        let drawn: Vec<Vec<u8>> = all.iter().map(|&g| draw(g, 0, 0.6)).collect();
        for (g, px) in all.iter().zip(&drawn) {
            assert_eq!(px.len(), PX * PX * 4);
            assert!(px.chunks(4).all(|p| p[..3] == [0, 0, 0]), "{g:?}: black only, macOS tints it");
            let ink = alpha(px).iter().filter(|&&a| a > 0).count();
            assert!(ink > 40 && ink < PX * PX / 2, "{g:?} draws a glyph, not a block ({ink} px)");
            // Clear of the edges, so nothing is clipped in the menu bar.
            assert!((0..PX).all(|i| px[i * 4 + 3] == 0 && px[((PX - 1) * PX + i) * 4 + 3] == 0), "{g:?} touches the top or bottom edge");
        }
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(drawn[i], drawn[j], "{:?} and {:?} look the same", all[i], all[j]);
            }
        }
    }

    #[test]
    fn the_waveform_follows_the_level_and_the_dots_take_turns() {
        let ink = |px: Vec<u8>| alpha(&px).iter().map(|&a| a as u32).sum::<u32>();
        assert!(ink(draw(Glyph::Wave, 3, 1.0)) > ink(draw(Glyph::Wave, 3, 0.1)), "louder is taller");
        assert_eq!(draw(Glyph::Wave, 3, 0.0), draw(Glyph::Wave, 4, 0.0), "silence holds still");
        assert_ne!(draw(Glyph::Working, 0, 0.0), draw(Glyph::Working, 1, 0.0));
        assert_eq!(draw(Glyph::Working, 0, 0.0), draw(Glyph::Working, 3, 0.0));
        assert_eq!(draw(Glyph::Idle, 0, 0.9), draw(Glyph::Idle, 7, 0.0), "idle never animates");
    }
}
