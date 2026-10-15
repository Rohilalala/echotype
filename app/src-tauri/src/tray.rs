//! Menu-bar mode (macOS): the status item is the live indicator instead of the pill. Its glyphs
//! are drawn here as monochrome template images, so macOS tints them for light and dark menu
//! bars, and change ~8 times a second only while listening or working.

use crate::settings::Settings;
use crate::{lock, state};
use std::f64::consts::{FRAC_PI_2, PI};
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
/// Frames per sweep of the working glyph's lit meridians (8 frames a second).
const SWEEP: u64 = 12;

/// A dot: centre, radius and opacity, in 2x pixels.
type Dot = (f64, f64, f64, f64);

/// The app icon's dotted sphere for 18 pt: a hexagonal grid of dots 5 px apart inside a circle,
/// sized by depth as thinking-orbs' globe (Jakub Antalik, MIT) sizes them, so it reads as a ball.
/// Each dot is (x, y, depth 0..1, longitude with 0 facing you).
fn globe() -> Vec<(f64, f64, f64, f64)> {
    const R: f64 = 14.5;
    let mut dots = Vec::new();
    for j in -3..=3_i32 {
        for i in -3..=3_i32 {
            let (dx, dy) = (5.0 * f64::from(i) + if j % 2 == 0 { 0.0 } else { 2.5 }, 4.5 * f64::from(j));
            let q = (dx * dx + dy * dy) / (R * R);
            if q <= 1.0 {
                let z = (1.0 - q).sqrt();
                dots.push((18.0 + dx, 18.0 + dy, z, (dx / R).atan2(z)));
            }
        }
    }
    dots
}

/// Dots evenly along a polyline, `n` of them including both ends.
fn along(path: &[(f64, f64)], n: usize, r: f64) -> Vec<Dot> {
    let seg: Vec<f64> = path.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).collect();
    let total: f64 = seg.iter().sum();
    (0..n)
        .map(|i| {
            let (mut d, mut j) = (total * i as f64 / (n - 1) as f64, 0);
            while j + 1 < seg.len() && d > seg[j] {
                d -= seg[j];
                j += 1;
            }
            let f = (d / seg[j]).min(1.0);
            let (a, b) = (path[j], path[j + 1]);
            ((a.0 + (b.0 - a.0) * f).round(), (a.1 + (b.1 - a.1) * f).round(), r, 1.0)
        })
        .collect()
}

/// Tiny deterministic jitter in 0..1 per frame and column, so a steady voice still looks alive.
fn jitter(frame: u64, i: usize) -> f32 {
    let mut x = frame.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (i as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    (x % 1000) as f32 / 1000.0
}

/// RGBA pixels (black, coverage in alpha) for `glyph` at animation `frame`.
pub fn draw(glyph: Glyph, frame: u64, meter: f32) -> Vec<u8> {
    let dots: Vec<Dot> = match glyph {
        // The sphere at rest, nearer dots bigger.
        Glyph::Idle => globe().into_iter().map(|(x, y, z, _)| (x, y, 0.8 + 1.3 * z, 1.0)).collect(),
        // The sphere opens from its equator with the level, each column on its own, like a waveform.
        Glyph::Wave => globe()
            .into_iter()
            .map(|(x, y, z, _)| {
                let col = ((x - 3.0) / 2.5).round() as usize;
                let w = 1.0 - ((x - 18.0) / 18.0).powi(2) as f32;
                let level = 15.0 * f64::from((meter * w * (0.75 + 0.5 * jitter(frame, col))).clamp(0.0, 1.0));
                (x, y, 0.8 + 1.3 * z, ((level - (y - 18.0).abs()) / 4.5 + 1.0).clamp(0.0, 1.0))
            })
            .collect(),
        // Two opposite meridians sweep the sphere, as the thinking-orbs globe scans while it searches.
        Glyph::Working => {
            let scan = PI * (frame % SWEEP) as f64 / SWEEP as f64;
            globe()
                .into_iter()
                .map(|(x, y, z, lon)| {
                    let off = (lon - scan + FRAC_PI_2).rem_euclid(PI) - FRAC_PI_2;
                    let lit = (-off * off / 0.18).exp();
                    (x, y, 0.8 + 1.3 * z * (0.6 + 0.4 * lit), 0.35 + 0.65 * lit)
                })
                .collect()
        }
        Glyph::Done => along(&[(8.0, 19.0), (15.0, 26.0), (28.0, 10.0)], 7, 2.2),
        // A triangle of dots around an exclamation mark.
        Glyph::Warning => {
            let mut d = along(&[(18.0, 4.0), (33.0, 31.0), (3.0, 31.0), (18.0, 4.0)], 16, 1.8);
            d.pop();
            d.extend([(18.0, 13.0, 1.9, 1.0), (18.0, 15.5, 1.9, 1.0), (18.0, 18.0, 1.9, 1.0), (18.0, 20.5, 1.7, 1.0), (18.0, 26.0, 2.1, 1.0)]);
            d
        }
    };
    let mut px = vec![0u8; PX * PX * 4];
    for y in 0..PX {
        for x in 0..PX {
            // 4x4 samples per pixel for smooth edges.
            let mut a = 0.0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let p = (x as f64 + (sx as f64 + 0.5) / 4.0, y as f64 + (sy as f64 + 0.5) / 4.0);
                    a += dots.iter().filter(|d| (p.0 - d.0).hypot(p.1 - d.1) <= d.2).map(|d| d.3).fold(0.0, f64::max);
                }
            }
            px[(y * PX + x) * 4 + 3] = (a / 16.0 * 255.0).round() as u8;
        }
    }
    px
}

/// The idle globe as a template image: the status item's icon whenever it is not a live indicator.
pub fn idle() -> Image<'static> {
    Image::new_owned(draw(Glyph::Idle, 0, 0.0), PX as u32, PX as u32)
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
    pub fn sync(&mut self, app: &AppHandle, s: &Settings, bar: bool, glyph: Glyph, meter: f32, line: Option<&str>, copy: bool) {
        let Some(tray) = app.tray_by_id("main") else { return };
        if self.mode != Some(bar) {
            // Left click dictates in the menu bar; the menu moves to right click.
            let _ = tray.set_show_menu_on_left_click(!bar);
            if !bar && self.mode.is_some() {
                let _ = tray.set_icon_with_as_template(Some(idle()), true);
                let _ = tray.set_menu(lock(&state(app).tray_menu).clone());
            }
            *self = Tray { mode: Some(bar), ..Tray::default() };
        }
        // The icon shows the live state in both modes: it moves with your voice while you dictate.
        let px = draw(glyph, crate::pill::now_ms() / 125, meter);
        if self.icon.as_ref() != Some(&px) {
            let _ = tray.set_icon_with_as_template(Some(Image::new_owned(px.clone(), PX as u32, PX as u32)), true);
            self.icon = Some(px);
        }
        if !bar {
            return; // the pill's own menu stays on the icon; only menu-bar mode rebuilds it
        }
        // ponytail: the microphone list is re-read every 5 s; a CoreAudio device listener if that ever lags.
        if self.mics.as_ref().is_none_or(|(t, _)| t.elapsed() > Duration::from_secs(5)) {
            self.mics = Some((Instant::now(), crate::audio::input_devices().into_iter().map(|d| d.name).collect()));
        }
        let mics = &self.mics.as_ref().expect("just filled").1;
        let recording = state(app).recording.load(std::sync::atomic::Ordering::SeqCst);
        let has_last = lock(&state(app).last_output).is_some();
        let key = format!("{line:?}|{copy}|{recording}|{has_last}|{:?}|{:?}|{mics:?}|{}", s.input_device, s.languages, crate::meeting::menu_label());
        if self.menu.as_ref() != Some(&key)
            && let Ok(m) = crate::pill::menu(app, mics, Some(crate::pill::Bar { line, recording, copy }))
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
    fn the_waveform_follows_the_level_and_the_meridians_sweep() {
        let ink = |px: Vec<u8>| alpha(&px).iter().map(|&a| a as u32).sum::<u32>();
        assert!(ink(draw(Glyph::Wave, 3, 1.0)) > ink(draw(Glyph::Wave, 3, 0.1)), "louder is taller");
        assert_eq!(draw(Glyph::Wave, 3, 0.0), draw(Glyph::Wave, 4, 0.0), "silence holds still");
        assert_ne!(draw(Glyph::Working, 0, 0.0), draw(Glyph::Working, 1, 0.0));
        assert_eq!(draw(Glyph::Working, 0, 0.0), draw(Glyph::Working, SWEEP, 0.0), "one sweep, then it repeats");
        assert_eq!(draw(Glyph::Idle, 0, 0.9), draw(Glyph::Idle, 7, 0.0), "idle never animates");
    }
}
