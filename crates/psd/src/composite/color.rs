//! Colour conversion at the boundary between native CMYK/Lab planes and RGB
//! compositing. ICC transforms are resolved once per document and reused for
//! every layer. RGB and grayscale documents retain their native sample space.

use std::sync::Arc;

use moxcms::{
    ColorProfile, DataColorSpace, InterpolationMethod, Layout, RenderingIntent,
    TransformF32Executor, TransformOptions,
};
use psd_core::{Color, ColorMode, Gradient, GradientKind, PsdError, Result, StopSource};

#[derive(Clone)]
pub(super) struct ColorContext {
    mode: ColorMode,
    transform: Option<Arc<TransformF32Executor>>,
}

impl ColorContext {
    pub fn new(mode: ColorMode, icc: &[u8]) -> Self {
        let expected = match mode {
            ColorMode::Cmyk => DataColorSpace::Cmyk,
            ColorMode::Lab => DataColorSpace::Lab,
            _ => {
                return Self {
                    mode,
                    transform: None,
                }
            }
        };
        let parsed = if icc.is_empty() {
            None
        } else if icc.len() > 16 * 1024 * 1024 {
            tracing::warn!("document ICC profile exceeds the conversion budget; using a fallback");
            None
        } else {
            match ColorProfile::new_from_slice(icc) {
                Ok(profile) if profile.color_space == expected => Some(profile),
                Ok(_) => {
                    tracing::warn!(
                        "document ICC profile does not match its colour mode; using a fallback"
                    );
                    None
                }
                Err(error) => {
                    tracing::warn!("unreadable document ICC profile: {error}; using a fallback");
                    None
                }
            }
        };
        let profile = parsed.or_else(|| {
            (mode == ColorMode::Lab).then(|| {
                let mut profile = ColorProfile::new_lab();
                // The virtual Lab profile has an identity A2B mapping. Its
                // samples therefore describe Lab PCS, so the transform must
                // insert Lab-to-XYZ rather than reading them as XYZ directly.
                profile.pcs = DataColorSpace::Lab;
                profile
            })
        });
        let transform = profile.and_then(|source| {
            let layout = if mode == ColorMode::Cmyk {
                Layout::Rgba
            } else {
                Layout::Rgb
            };
            let destination = ColorProfile::new_srgb();
            let options = TransformOptions {
                rendering_intent: RenderingIntent::RelativeColorimetric,
                interpolation_method: InterpolationMethod::Tetrahedral,
                prefer_fixed_point: false,
                ..TransformOptions::default()
            };
            source
                .create_transform_f32(layout, &destination, Layout::Rgb, options)
                .or_else(|_| {
                    source.create_transform_f32(
                        layout,
                        &destination,
                        Layout::Rgb,
                        TransformOptions {
                            rendering_intent: RenderingIntent::Perceptual,
                            ..options
                        },
                    )
                })
                .map_err(|error| {
                    tracing::warn!("unsupported document ICC transform: {error}; using a fallback")
                })
                .ok()
        });
        Self { mode, transform }
    }

    pub fn convert_planar(&self, mut planes: Vec<Vec<f32>>, depth: u16) -> Result<[Vec<f32>; 3]> {
        let Some(first) = planes.first() else {
            return Err(invalid());
        };
        let pixels = first.len();
        let components = match self.mode {
            ColorMode::Cmyk => 4,
            ColorMode::Grayscale
            | ColorMode::Bitmap
            | ColorMode::Duotone
            | ColorMode::Multichannel => 1,
            _ => 3,
        };
        if planes.len() != components || planes.iter().any(|p| p.len() != pixels) {
            return Err(invalid());
        }
        if let Some(transform) = &self.transform {
            // A small reusable interleaved batch keeps the transform workspace
            // bounded instead of copying the whole planar image.
            let mut source = vec![0.0f32; components * 1024];
            let mut target = vec![0.0f32; 3 * 1024];
            for start in (0..pixels).step_by(1024) {
                let count = (pixels - start).min(1024);
                for i in 0..count {
                    for channel in 0..components {
                        let mut value = planes[channel][start + i];
                        if self.mode == ColorMode::Cmyk {
                            // PSD planes are inverse ink; ICC CMYK expects ink.
                            value = 1.0 - value;
                        } else if self.mode == ColorMode::Lab && channel > 0 && depth == 16 {
                            // 16-bit Lab chroma is biased at 32768 and uses 256
                            // integer units per a*/b* unit; ICC normalizes 255.
                            value *= 65535.0 / 65280.0;
                        }
                        source[i * components + channel] = if value.is_finite() {
                            value.clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                    }
                }
                transform
                    .transform(&source[..count * components], &mut target[..count * 3])
                    .map_err(|e| {
                        PsdError::ImageDecode(format!("ICC colour conversion failed: {e}"))
                    })?;
                for i in 0..count {
                    for channel in 0..3 {
                        planes[channel][start + i] = target[i * 3 + channel].clamp(0.0, 1.0);
                    }
                }
            }
            planes.truncate(3);
            return planes.try_into().map_err(|_| invalid());
        }
        if components == 1 {
            return Ok([planes[0].clone(), planes[0].clone(), planes[0].clone()]);
        }
        if self.mode == ColorMode::Cmyk {
            let [mut c, mut m, mut y, black]: [Vec<f32>; 4] =
                planes.try_into().map_err(|_| invalid())?;
            for plane in [&mut c, &mut m, &mut y] {
                for (value, black) in plane.iter_mut().zip(&black) {
                    *value *= black;
                }
            }
            return Ok([c, m, y]);
        }
        if self.mode == ColorMode::Lab {
            let [mut l, mut a, mut b]: [Vec<f32>; 3] = planes.try_into().map_err(|_| invalid())?;
            for ((l, a), b) in l.iter_mut().zip(&mut a).zip(&mut b) {
                let scale = if depth == 16 { 65535.0 / 256.0 } else { 255.0 };
                let color = super::ramp::color_rgb(&Color::Lab {
                    lightness: f64::from(*l) * 100.0,
                    a: f64::from(*a) * scale - 128.0,
                    b: f64::from(*b) * scale - 128.0,
                });
                (*l, *a, *b) = (color[0], color[1], color[2]);
            }
            return Ok([l, a, b]);
        }
        planes.try_into().map_err(|_| invalid())
    }

    pub fn color(&self, color: &Color) -> [f32; 3] {
        let native = match (self.mode, color) {
            (
                ColorMode::Cmyk,
                Color::Cmyk {
                    cyan,
                    magenta,
                    yellow,
                    black,
                },
            ) => Some(
                vec![*cyan, *magenta, *yellow, *black]
                    .into_iter()
                    .map(|ink| vec![1.0 - ink as f32 / 100.0])
                    .collect(),
            ),
            (ColorMode::Lab, Color::Lab { lightness, a, b }) => Some(vec![
                vec![*lightness as f32 / 100.0],
                vec![(*a as f32 + 128.0) / 255.0],
                vec![(*b as f32 + 128.0) / 255.0],
            ]),
            _ => None,
        };
        if let Some(native) = native {
            if let Ok(converted) = self.convert_planar(native, 8) {
                return [converted[0][0], converted[1][0], converted[2][0]];
            }
        }
        super::ramp::color_rgb(color)
    }

    pub fn gradient(&self, gradient: &Gradient) -> Gradient {
        let mut gradient = gradient.clone();
        if let GradientKind::Solid(solid) = &mut gradient.kind {
            for stop in &mut solid.color_stops {
                if let StopSource::User(color) = stop.source {
                    let rgb = self.color(&color);
                    stop.source = StopSource::User(Color::Rgb {
                        red: f64::from(rgb[0]) * 255.0,
                        green: f64::from(rgb[1]) * 255.0,
                        blue: f64::from(rgb[2]) * 255.0,
                    });
                }
            }
        }
        gradient
    }
}

fn invalid() -> PsdError {
    PsdError::InvalidData {
        offset: 0,
        message: "native colour planes have inconsistent dimensions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lab_d50_converts_to_srgb_without_an_embedded_profile() {
        let context = ColorContext::new(ColorMode::Lab, &[]);
        let rgb = context.color(&Color::Lab {
            lightness: 60.0,
            a: 25.0,
            b: 25.0,
        });
        for (v, expected) in rgb.into_iter().zip([196.0, 126.0, 101.0]) {
            assert!((v * 255.0 - expected).abs() < 2.0, "{rgb:?}");
        }
    }

    #[test]
    fn invalid_and_mismatched_profiles_fall_back_without_panicking() {
        for bytes in [vec![1; 140], ColorProfile::new_srgb().encode().unwrap()] {
            let context = ColorContext::new(ColorMode::Cmyk, &bytes);
            let rgb = context
                .convert_planar(vec![vec![0.5], vec![0.25], vec![1.0], vec![0.5]], 8)
                .unwrap();
            assert_eq!([rgb[0][0], rgb[1][0], rgb[2][0]], [0.25, 0.125, 0.5]);
        }
    }

    #[test]
    fn eight_and_sixteen_bit_lab_use_the_same_neutral_chroma() {
        let context = ColorContext::new(ColorMode::Lab, &[]);
        let eight = context
            .convert_planar(vec![vec![0.6], vec![128.0 / 255.0], vec![128.0 / 255.0]], 8)
            .unwrap();
        let sixteen = context
            .convert_planar(
                vec![vec![0.6], vec![32768.0 / 65535.0], vec![32768.0 / 65535.0]],
                16,
            )
            .unwrap();
        for channel in 0..3 {
            assert!((eight[channel][0] - sixteen[channel][0]).abs() < 1e-5);
        }
    }
}
