//! Gradients as descriptors carry them (`Grdn`).
//!
//! Gradient overlays, gradient strokes, gradient outer glows and gradient-fill layers all embed
//! the same descriptor: a name, a gradient form (`GrdF`: custom stops `CstS` or noise `ClNs`)
//! and, for a custom gradient, a colour-stop list and a transparency-stop list. Locations are
//! on Photoshop's 0-4096 scale and midpoints are percentages, as stored.
//!
//! The descriptor layouts follow Photoshop-authored files. Where a value is not modelled
//! (`from_descriptor` returns `None`), callers leave the descriptor as it was.

use crate::color::Color;
use crate::descriptor::{Descriptor, DescriptorValue};

const STOP_ORDER: [&str; 4] = ["Clr ", "Type", "Lctn", "Mdpn"];
const TRANSPARENCY_ORDER: [&str; 3] = ["Opct", "Lctn", "Mdpn"];
const SOLID_ORDER: [&str; 5] = ["Nm  ", "GrdF", "Intr", "Clrs", "Trns"];
const NOISE_ORDER: [&str; 9] = [
    "Nm  ", "GrdF", "ShTr", "VctC", "ClrS", "RndS", "Smth", "Mnm ", "Mxm ",
];

/// Where a colour stop gets its colour.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StopSource {
    /// `UsrS`: a colour stored in the stop.
    User(Color),
    /// `FrgC`: the foreground colour when the gradient is used.
    Foreground,
    /// `BckC`: the background colour when the gradient is used.
    Background,
}

/// One colour stop of a custom gradient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorStop {
    pub source: StopSource,
    /// `Lctn`: position, 0-4096.
    pub location: i32,
    /// `Mdpn`: midpoint towards the next stop, a percentage (50 is centred).
    pub midpoint: i32,
}

/// One transparency stop of a custom gradient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransparencyStop {
    /// `Opct`: opacity as a percentage.
    pub opacity: f64,
    pub location: i32,
    pub midpoint: i32,
}

/// A custom gradient: colour stops and transparency stops.
#[derive(Debug, Clone, PartialEq)]
pub struct SolidGradient {
    /// `Intr`: smoothness, 4096 for a fully smooth gradient.
    pub smoothness: f64,
    pub color_stops: Vec<ColorStop>,
    pub transparency_stops: Vec<TransparencyStop>,
}

/// The colour model a noise gradient varies its colours in (`ClrS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseColorModel {
    Rgb,
    Hsb,
    Lab,
}

impl NoiseColorModel {
    fn id(self) -> &'static str {
        match self {
            Self::Rgb => "RGBC",
            Self::Hsb => "HSBl",
            Self::Lab => "LbCl",
        }
    }

    fn from_id(id: &[u8]) -> Option<Self> {
        Some(match id {
            b"RGBC" => Self::Rgb,
            b"HSBl" => Self::Hsb,
            b"LbCl" => Self::Lab,
            _ => return None,
        })
    }
}

/// A noise gradient: random colours within per-channel ranges.
#[derive(Debug, Clone, PartialEq)]
pub struct NoiseGradient {
    /// `VctC`: restrict colours.
    pub restrict_colors: bool,
    /// `ShTr`: add transparency.
    pub add_transparency: bool,
    pub color_model: NoiseColorModel,
    /// `RndS`: the random seed.
    pub seed: i32,
    /// `Smth`: roughness.
    pub roughness: i32,
    /// `Mnm `: the per-channel minimums.
    pub minimum: Vec<i32>,
    /// `Mxm `: the per-channel maximums.
    pub maximum: Vec<i32>,
}

/// The two forms a gradient takes.
#[derive(Debug, Clone, PartialEq)]
pub enum GradientKind {
    Solid(SolidGradient),
    Noise(NoiseGradient),
}

/// A `Grdn` descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct Gradient {
    /// The descriptor's own name, which Photoshop localises (`Gradient`, and so on).
    pub label: String,
    /// `Nm  `: the gradient's name.
    pub name: String,
    pub kind: GradientKind,
}

impl Gradient {
    /// A smooth two-stop black-to-white gradient, the default Photoshop starts a new gradient
    /// overlay from.
    pub fn black_to_white() -> Self {
        let stop = |red: f64, location| ColorStop {
            source: StopSource::User(Color::Rgb {
                red,
                green: red,
                blue: red,
            }),
            location,
            midpoint: 50,
        };
        let opaque = |location| TransparencyStop {
            opacity: 100.0,
            location,
            midpoint: 50,
        };
        Self {
            label: "Gradient".to_owned(),
            name: "Custom".to_owned(),
            kind: GradientKind::Solid(SolidGradient {
                smoothness: 4096.0,
                color_stops: vec![stop(0.0, 0), stop(255.0, 4096)],
                transparency_stops: vec![opaque(0), opaque(4096)],
            }),
        }
    }

    /// Read a `Grdn` descriptor, or `None` when its form or any stop is not one this type
    /// models.
    pub fn from_descriptor(descriptor: &Descriptor) -> Option<Self> {
        let kind = match descriptor.get_enum("GrdF")? {
            b"CstS" => GradientKind::Solid(read_solid(descriptor)?),
            b"ClNs" => GradientKind::Noise(read_noise(descriptor)?),
            _ => return None,
        };
        Some(Self {
            label: descriptor.name.value().trim_end_matches('\0').to_owned(),
            name: descriptor.get_text("Nm  ")?,
            kind,
        })
    }

    /// The gradient as a fresh descriptor.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("Grdn");
        descriptor.set_name(&self.label);
        self.patch(&mut descriptor);
        descriptor
    }

    /// Write this gradient into `descriptor`. A gradient of the same form is edited in place,
    /// so unknown items and the stops' unmodelled fields survive; a different form replaces
    /// the descriptor.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let same_form = Self::from_descriptor(descriptor).is_some_and(|current| {
            std::mem::discriminant(&current.kind) == std::mem::discriminant(&self.kind)
        });
        if same_form {
            descriptor.set_name(&self.label);
            self.patch(descriptor);
        } else {
            *descriptor = self.to_descriptor();
        }
    }

    fn patch(&self, d: &mut Descriptor) {
        match &self.kind {
            GradientKind::Solid(solid) => {
                let order = &SOLID_ORDER;
                d.set_text_ordered("Nm  ", &self.name, order);
                d.set_ordered("GrdF", DescriptorValue::enumerated("GrdF", "CstS"), order);
                d.set_ordered("Intr", DescriptorValue::double(solid.smoothness), order);
                let colors = patch_list(d, "Clrs", solid.color_stops.len(), "Clrt", |stop, i| {
                    write_color_stop(stop, &solid.color_stops[i]);
                });
                d.set_ordered("Clrs", colors, order);
                let transparency = patch_list(
                    d,
                    "Trns",
                    solid.transparency_stops.len(),
                    "TrnS",
                    |stop, i| {
                        write_transparency_stop(stop, &solid.transparency_stops[i]);
                    },
                );
                d.set_ordered("Trns", transparency, order);
            }
            GradientKind::Noise(noise) => {
                let order = &NOISE_ORDER;
                d.set_text_ordered("Nm  ", &self.name, order);
                d.set_ordered("GrdF", DescriptorValue::enumerated("GrdF", "ClNs"), order);
                d.set_ordered(
                    "ShTr",
                    DescriptorValue::boolean(noise.add_transparency),
                    order,
                );
                d.set_ordered(
                    "VctC",
                    DescriptorValue::boolean(noise.restrict_colors),
                    order,
                );
                let model = DescriptorValue::enumerated("ClrS", noise.color_model.id());
                d.set_ordered("ClrS", model, order);
                d.set_ordered("RndS", DescriptorValue::long(noise.seed), order);
                d.set_ordered("Smth", DescriptorValue::long(noise.roughness), order);
                let longs = |values: &[i32]| {
                    DescriptorValue::List(
                        values.iter().map(|v| DescriptorValue::long(*v)).collect(),
                    )
                };
                d.set_ordered("Mnm ", longs(&noise.minimum), order);
                d.set_ordered("Mxm ", longs(&noise.maximum), order);
            }
        }
    }
}

/// Rebuild the list at `key` with `count` entries, each an existing descriptor (so its
/// unmodelled items survive) or a fresh one of class `class`, edited by `write`.
fn patch_list(
    d: &Descriptor,
    key: &str,
    count: usize,
    class: &str,
    mut write: impl FnMut(&mut Descriptor, usize),
) -> DescriptorValue {
    let existing: &[DescriptorValue] = d
        .get(key)
        .and_then(DescriptorValue::as_list)
        .unwrap_or_default();
    let items = (0..count)
        .map(|index| {
            let mut item = match existing.get(index) {
                Some(DescriptorValue::Descriptor(current)) => current.clone(),
                _ => Descriptor::with_class(class),
            };
            write(&mut item, index);
            DescriptorValue::Descriptor(item)
        })
        .collect();
    DescriptorValue::List(items)
}

fn write_color_stop(d: &mut Descriptor, stop: &ColorStop) {
    let kind = match stop.source {
        StopSource::User(_) => "UsrS",
        StopSource::Foreground => "FrgC",
        StopSource::Background => "BckC",
    };
    match stop.source {
        StopSource::User(color) => {
            let mut child = match d.remove("Clr ") {
                Some(DescriptorValue::Descriptor(current)) => current,
                _ => Descriptor::with_class(color.class_id()),
            };
            color.apply_to(&mut child);
            d.set_ordered("Clr ", DescriptorValue::Descriptor(child), &STOP_ORDER);
        }
        StopSource::Foreground | StopSource::Background => {
            d.remove("Clr ");
        }
    }
    d.set_ordered(
        "Type",
        DescriptorValue::enumerated("Clry", kind),
        &STOP_ORDER,
    );
    d.set_ordered("Lctn", DescriptorValue::long(stop.location), &STOP_ORDER);
    d.set_ordered("Mdpn", DescriptorValue::long(stop.midpoint), &STOP_ORDER);
}

fn write_transparency_stop(d: &mut Descriptor, stop: &TransparencyStop) {
    d.set_ordered(
        "Opct",
        DescriptorValue::percent(stop.opacity),
        &TRANSPARENCY_ORDER,
    );
    d.set_ordered(
        "Lctn",
        DescriptorValue::long(stop.location),
        &TRANSPARENCY_ORDER,
    );
    d.set_ordered(
        "Mdpn",
        DescriptorValue::long(stop.midpoint),
        &TRANSPARENCY_ORDER,
    );
}

fn read_solid(d: &Descriptor) -> Option<SolidGradient> {
    let color_stops = d
        .get("Clrs")?
        .as_list()?
        .iter()
        .map(|value| {
            let stop = value.as_descriptor()?;
            let source = match stop.get_enum("Type")? {
                b"UsrS" => {
                    StopSource::User(Color::from_descriptor(stop.get("Clr ")?.as_descriptor()?)?)
                }
                b"FrgC" => StopSource::Foreground,
                b"BckC" => StopSource::Background,
                _ => return None,
            };
            Some(ColorStop {
                source,
                location: stop.get_long("Lctn")?,
                midpoint: stop.get_long("Mdpn")?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let transparency_stops = d
        .get("Trns")?
        .as_list()?
        .iter()
        .map(|value| {
            let stop = value.as_descriptor()?;
            Some(TransparencyStop {
                opacity: stop.get_unit("Opct", *b"#Prc")?,
                location: stop.get_long("Lctn")?,
                midpoint: stop.get_long("Mdpn")?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(SolidGradient {
        smoothness: crate::color::number(d, "Intr")?,
        color_stops,
        transparency_stops,
    })
}

fn read_noise(d: &Descriptor) -> Option<NoiseGradient> {
    let longs = |key: &str| -> Option<Vec<i32>> {
        d.get(key)?
            .as_list()?
            .iter()
            .map(DescriptorValue::as_integer)
            .collect()
    };
    Some(NoiseGradient {
        restrict_colors: d.get_bool("VctC")?,
        add_transparency: d.get_bool("ShTr")?,
        color_model: NoiseColorModel::from_id(d.get_enum("ClrS")?)?,
        seed: d.get_long("RndS")?,
        roughness: d.get_long("Smth")?,
        minimum: longs("Mnm ")?,
        maximum: longs("Mxm ")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::{DescriptorItem, DescriptorKey};

    fn noise() -> Gradient {
        Gradient {
            label: "Gradient".to_owned(),
            name: "Noise".to_owned(),
            kind: GradientKind::Noise(NoiseGradient {
                restrict_colors: true,
                add_transparency: false,
                color_model: NoiseColorModel::Rgb,
                seed: 1_234_567,
                roughness: 50,
                minimum: vec![0, 0, 0, 0],
                maximum: vec![100, 100, 100, 100],
            }),
        }
    }

    #[test]
    fn a_solid_gradient_round_trips_and_uses_photoshops_stop_layout() {
        let gradient = Gradient::black_to_white();
        let d = gradient.to_descriptor();
        assert_eq!(Gradient::from_descriptor(&d), Some(gradient));

        let keys = |d: &Descriptor| -> Vec<String> {
            d.items
                .iter()
                .map(|i| i.key.as_str().trim_end().to_owned())
                .collect()
        };
        assert_eq!(keys(&d), ["Nm", "GrdF", "Intr", "Clrs", "Trns"]);
        let stop = d.get("Clrs").unwrap().as_list().unwrap()[0]
            .as_descriptor()
            .unwrap();
        assert_eq!(keys(stop), ["Clr", "Type", "Lctn", "Mdpn"]);
        let opacity = d.get("Trns").unwrap().as_list().unwrap()[0]
            .as_descriptor()
            .unwrap();
        assert_eq!(keys(opacity), ["Opct", "Lctn", "Mdpn"]);
    }

    #[test]
    fn a_noise_gradient_round_trips() {
        let gradient = noise();
        assert_eq!(
            Gradient::from_descriptor(&gradient.to_descriptor()),
            Some(gradient)
        );
    }

    #[test]
    fn foreground_and_background_stops_carry_no_colour() {
        let mut gradient = Gradient::black_to_white();
        let GradientKind::Solid(solid) = &mut gradient.kind else {
            unreachable!()
        };
        solid.color_stops[0].source = StopSource::Foreground;
        solid.color_stops[1].source = StopSource::Background;
        let d = gradient.to_descriptor();
        let stop = d.get("Clrs").unwrap().as_list().unwrap()[0]
            .as_descriptor()
            .unwrap();
        assert!(!stop.contains("Clr "));
        assert_eq!(Gradient::from_descriptor(&d), Some(gradient));
    }

    #[test]
    fn patching_keeps_unknown_items_at_gradient_and_stop_level() {
        let mut d = Gradient::black_to_white().to_descriptor();
        d.items.push(DescriptorItem {
            key: DescriptorKey::new("gradientLevelExtra"),
            value: DescriptorValue::long(1),
        });
        let DescriptorValue::List(stops) = d.get_mut("Clrs").unwrap() else {
            unreachable!()
        };
        let DescriptorValue::Descriptor(first) = &mut stops[0] else {
            unreachable!()
        };
        first.set("stopLevelExtra", DescriptorValue::long(2));

        let mut edited = Gradient::black_to_white();
        let GradientKind::Solid(solid) = &mut edited.kind else {
            unreachable!()
        };
        solid.color_stops[0].location = 100;
        solid.color_stops.push(ColorStop {
            source: StopSource::Foreground,
            location: 2048,
            midpoint: 50,
        });
        edited.apply_to(&mut d);

        assert_eq!(Gradient::from_descriptor(&d), Some(edited));
        assert!(d.contains("gradientLevelExtra"));
        let stops = d.get("Clrs").unwrap().as_list().unwrap();
        assert!(stops[0].as_descriptor().unwrap().contains("stopLevelExtra"));
        assert_eq!(stops.len(), 3);
    }

    #[test]
    fn changing_form_replaces_the_descriptor() {
        let mut d = Gradient::black_to_white().to_descriptor();
        noise().apply_to(&mut d);
        assert_eq!(Gradient::from_descriptor(&d), Some(noise()));
        assert!(!d.contains("Clrs"));
    }

    #[test]
    fn an_unmodelled_form_or_stop_reads_as_none() {
        let mut d = Gradient::black_to_white().to_descriptor();
        d.set("GrdF", DescriptorValue::enumerated("GrdF", "Mystery"));
        assert_eq!(Gradient::from_descriptor(&d), None);

        let mut d = Gradient::black_to_white().to_descriptor();
        let DescriptorValue::List(stops) = d.get_mut("Clrs").unwrap() else {
            unreachable!()
        };
        let DescriptorValue::Descriptor(first) = &mut stops[0] else {
            unreachable!()
        };
        first.remove("Lctn");
        assert_eq!(Gradient::from_descriptor(&d), None);
    }
}
