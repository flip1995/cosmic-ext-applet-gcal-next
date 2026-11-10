//! Visual helpers: theme color resolution and the pulse animation.

use std::f32::consts::TAU;

use cosmic::iced::Color;

use crate::config::{self, ResolvedStyle, ThemeColor};

/// Opacity factor for `dim` styles.
const DIM_ALPHA: f32 = 0.55;

/// Background alpha while pulsing:
/// `min + (1 − min) · (0.5 + 0.5 · sin(2π · t / period))`.
pub fn pulse_alpha(elapsed_ms: u64, period_ms: u32, min_alpha: f32) -> f32 {
    let min = min_alpha.clamp(0.0, 1.0);
    let period = u64::from(period_ms.max(1));
    let phase = (elapsed_ms % period) as f32 / period as f32;
    min + (1.0 - min) * (0.5 + 0.5 * (TAU * phase).sin())
}

/// Black or white, whichever is more readable on the given sRGB color.
pub fn contrast_text([r, g, b, _]: [u8; 4]) -> [u8; 3] {
    let linear = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
    // Crossover where black and white have equal contrast ratio.
    if luminance > 0.179 {
        [0, 0, 0]
    } else {
        [255, 255, 255]
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PillColors {
    pub background: Color,
    pub text: Color,
}

/// Colors of the pill for `style` in the active theme. `pulse` (0..=1)
/// scales the background alpha.
pub fn pill_colors(style: &ResolvedStyle, theme: &cosmic::Theme, pulse: f32) -> PillColors {
    let cosmic = theme.cosmic();
    let (mut background, mut text): (Color, Color) = match style.color {
        config::Color::Theme(ThemeColor::Neutral) => {
            let component = &cosmic.background(theme.transparent).component;
            (component.base.into(), component.on.into())
        }
        config::Color::Theme(ThemeColor::Accent) => (
            cosmic.accent_color().into(),
            cosmic.on_accent_color().into(),
        ),
        config::Color::Theme(ThemeColor::Success) => (
            cosmic.success_color().into(),
            cosmic.on_success_color().into(),
        ),
        config::Color::Theme(ThemeColor::Warning) => (
            cosmic.warning_color().into(),
            cosmic.on_warning_color().into(),
        ),
        config::Color::Theme(ThemeColor::Destructive) => (
            cosmic.destructive_color().into(),
            cosmic.on_destructive_color().into(),
        ),
        config::Color::Rgba(rgba @ [r, g, b, a]) => {
            let [tr, tg, tb] = contrast_text(rgba);
            (
                Color::from_rgba8(r, g, b, f32::from(a) / 255.0),
                Color::from_rgb8(tr, tg, tb),
            )
        }
    };
    let dim = if style.dim { DIM_ALPHA } else { 1.0 };
    background.a *= pulse.clamp(0.0, 1.0) * dim;
    text.a *= dim;
    PillColors { background, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_range_and_period() {
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert!(close(pulse_alpha(0, 1200, 0.35), 0.675));
        assert!(close(pulse_alpha(300, 1200, 0.35), 1.0));
        assert!(close(pulse_alpha(900, 1200, 0.35), 0.35));
        assert!(close(pulse_alpha(1500, 1200, 0.35), 1.0));
        for t in 0..2400 {
            let a = pulse_alpha(t, 1200, 0.35);
            assert!((0.35 - 1e-4..=1.0 + 1e-4).contains(&a));
        }
        // Degenerate config must not panic.
        assert!(pulse_alpha(10, 0, 2.0).is_finite());
    }

    #[test]
    fn readable_text_colors() {
        assert_eq!(contrast_text([0xf5, 0x84, 0x1f, 0xff]), [0, 0, 0]);
        assert_eq!(contrast_text([0xff, 0xff, 0x00, 0xff]), [0, 0, 0]);
        assert_eq!(contrast_text([0x20, 0x20, 0x60, 0xff]), [255, 255, 255]);
        assert_eq!(contrast_text([0xc0, 0x1c, 0x28, 0xff]), [255, 255, 255]);
    }
}
