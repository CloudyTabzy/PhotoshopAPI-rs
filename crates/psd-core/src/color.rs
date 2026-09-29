//! Colours as action descriptors carry them (`RGBC`, `CMYC`, `Grsc`, `HSBC`, `LbCl`).
//!
//! Effects, gradient stops and fill layers all store a colour as a small descriptor whose
//! class names the colour model. Photoshop-authored files use `RGBC` and `CMYC`; the class
//! IDs for the other three are Photoshop's action-manager terminology. The public
//! `ag-psd-rs` reference (<https://github.com/Vasyanator/ag-psd-rs>) names the gray and Lab
//! classes `GRYC` and `LABC` instead; no file in the corpora used either, so this reads both
//! spellings and writes Photoshop's own.

use crate::descriptor::{Descriptor, DescriptorValue};

/// A colour in one of the models a descriptor can name.
///
/// Components are stored as Photoshop stores them: RGB as 0-255, CMYK, gray and HSB
/// saturation/brightness as percentages, hue in degrees, Lab as its usual ranges.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// `RGBC`: `Rd  `, `Grn `, `Bl  `.
    Rgb { red: f64, green: f64, blue: f64 },
    /// `CMYC`: `Cyn `, `Mgnt`, `Ylw `, `Blck`.
    Cmyk {
        cyan: f64,
        magenta: f64,
        yellow: f64,
        black: f64,
    },
    /// `Grsc`: `Gry `.
    Gray { gray: f64 },
    /// `HSBC`: `H   ` (an angle), `Strt`, `Brgh`.
    Hsb {
        hue: f64,
        saturation: f64,
        brightness: f64,
    },
    /// `LbCl`: `Lmnc`, `A   `, `B   `.
    Lab { lightness: f64, a: f64, b: f64 },
}

/// Component values of a numeric descriptor field, whatever its OSType.
///
/// Photoshop writes colour components as `doub`, but a hand-built or older file may use
/// `long` or a unit float; all read as the number they hold.
pub(crate) fn number(descriptor: &Descriptor, key: &str) -> Option<f64> {
    match descriptor.get(key)? {
        DescriptorValue::Double(value) => Some(*value),
        DescriptorValue::Integer(value) => Some(f64::from(*value)),
        DescriptorValue::UnitFloat { value, .. } => Some(*value),
        _ => None,
    }
}

impl Color {
    /// The class ID of this colour's model.
    pub fn class_id(&self) -> &'static str {
        match self {
            Self::Rgb { .. } => "RGBC",
            Self::Cmyk { .. } => "CMYC",
            Self::Gray { .. } => "Grsc",
            Self::Hsb { .. } => "HSBC",
            Self::Lab { .. } => "LbCl",
        }
    }

    /// Read a colour descriptor, or `None` if its shape is not one this type models
    /// (a floating-point RGB colour, say, or a colour that is missing a component).
    pub fn from_descriptor(descriptor: &Descriptor) -> Option<Self> {
        let n = |key| number(descriptor, key);
        if descriptor.contains("Rd  ") {
            Some(Self::Rgb {
                red: n("Rd  ")?,
                green: n("Grn ")?,
                blue: n("Bl  ")?,
            })
        } else if descriptor.contains("Cyn ") {
            Some(Self::Cmyk {
                cyan: n("Cyn ")?,
                magenta: n("Mgnt")?,
                yellow: n("Ylw ")?,
                black: n("Blck")?,
            })
        } else if descriptor.contains("Gry ") {
            Some(Self::Gray { gray: n("Gry ")? })
        } else if descriptor.contains("H   ") {
            Some(Self::Hsb {
                hue: n("H   ")?,
                saturation: n("Strt")?,
                brightness: n("Brgh")?,
            })
        } else if descriptor.contains("Lmnc") {
            Some(Self::Lab {
                lightness: n("Lmnc")?,
                a: n("A   ")?,
                b: n("B   ")?,
            })
        } else {
            None
        }
    }

    /// The colour as a fresh descriptor.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class(self.class_id());
        self.write_components(&mut descriptor);
        descriptor
    }

    /// Write this colour into `descriptor`, editing it in place when it already holds a
    /// colour of the same model (so unknown items and key encodings survive) and replacing it
    /// with a fresh one when the model changes.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let same_model = Self::from_descriptor(descriptor).is_some_and(|current| {
            std::mem::discriminant(&current) == std::mem::discriminant(self)
        });
        if same_model {
            self.write_components(descriptor);
        } else {
            *descriptor = self.to_descriptor();
        }
    }

    fn write_components(&self, d: &mut Descriptor) {
        let double = DescriptorValue::double;
        match *self {
            Self::Rgb { red, green, blue } => {
                d.set("Rd  ", double(red));
                d.set("Grn ", double(green));
                d.set("Bl  ", double(blue));
            }
            Self::Cmyk {
                cyan,
                magenta,
                yellow,
                black,
            } => {
                d.set("Cyn ", double(cyan));
                d.set("Mgnt", double(magenta));
                d.set("Ylw ", double(yellow));
                d.set("Blck", double(black));
            }
            Self::Gray { gray } => d.set("Gry ", double(gray)),
            Self::Hsb {
                hue,
                saturation,
                brightness,
            } => {
                d.set("H   ", DescriptorValue::angle(hue));
                d.set("Strt", double(saturation));
                d.set("Brgh", double(brightness));
            }
            Self::Lab { lightness, a, b } => {
                d.set("Lmnc", double(lightness));
                d.set("A   ", double(a));
                d.set("B   ", double(b));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::DescriptorKey;

    fn all() -> [Color; 5] {
        [
            Color::Rgb {
                red: 255.0,
                green: 128.5,
                blue: 0.0,
            },
            Color::Cmyk {
                cyan: 10.0,
                magenta: 20.0,
                yellow: 30.0,
                black: 40.0,
            },
            Color::Gray { gray: 50.0 },
            Color::Hsb {
                hue: 210.0,
                saturation: 60.0,
                brightness: 70.0,
            },
            Color::Lab {
                lightness: 55.0,
                a: -12.0,
                b: 30.0,
            },
        ]
    }

    #[test]
    fn every_model_round_trips_through_a_descriptor() {
        for color in all() {
            let descriptor = color.to_descriptor();
            assert_eq!(descriptor.class_id.as_str(), color.class_id());
            assert_eq!(
                Color::from_descriptor(&descriptor),
                Some(color),
                "{color:?}"
            );
        }
    }

    #[test]
    fn a_fresh_rgb_colour_uses_character_ids() {
        let descriptor = Color::Rgb {
            red: 1.0,
            green: 2.0,
            blue: 3.0,
        }
        .to_descriptor();
        let keys: Vec<_> = descriptor
            .items
            .iter()
            .map(|i| i.key.as_bytes().to_vec())
            .collect();
        assert_eq!(keys, [b"Rd  ".to_vec(), b"Grn ".to_vec(), b"Bl  ".to_vec()]);
        assert!(descriptor
            .items
            .iter()
            .all(|i| i.key.uses_implicit_length()));
        assert!(descriptor.class_id.uses_implicit_length());
    }

    #[test]
    fn components_read_whatever_number_type_holds_them() {
        let mut d = Descriptor::with_class("RGBC");
        d.set("Rd  ", DescriptorValue::long(255));
        d.set("Grn ", DescriptorValue::double(1.5));
        d.set("Bl  ", DescriptorValue::percent(2.0));
        assert_eq!(
            Color::from_descriptor(&d),
            Some(Color::Rgb {
                red: 255.0,
                green: 1.5,
                blue: 2.0
            })
        );
    }

    #[test]
    fn the_alternative_class_spellings_read_by_their_components() {
        // Only the components matter, so a `GRYC` or `LABC` class still reads.
        let mut gray = Descriptor::with_class("GRYC");
        gray.set("Gry ", DescriptorValue::double(9.0));
        assert_eq!(
            Color::from_descriptor(&gray),
            Some(Color::Gray { gray: 9.0 })
        );
        let mut lab = Descriptor::with_class("LABC");
        lab.set("Lmnc", DescriptorValue::double(1.0));
        lab.set("A   ", DescriptorValue::double(2.0));
        lab.set("B   ", DescriptorValue::double(3.0));
        assert_eq!(
            Color::from_descriptor(&lab),
            Some(Color::Lab {
                lightness: 1.0,
                a: 2.0,
                b: 3.0
            })
        );
    }

    #[test]
    fn a_shape_it_does_not_model_reads_as_none() {
        let mut float_rgb = Descriptor::with_class("RGBC");
        float_rgb.set("redFloat", DescriptorValue::double(1.0));
        assert_eq!(Color::from_descriptor(&float_rgb), None);
        // A missing component is malformed, not a zero.
        let mut partial = Descriptor::with_class("RGBC");
        partial.set("Rd  ", DescriptorValue::double(1.0));
        assert_eq!(Color::from_descriptor(&partial), None);
    }

    #[test]
    fn apply_edits_in_place_within_a_model_and_replaces_across_models() {
        let mut d = Color::Rgb {
            red: 1.0,
            green: 2.0,
            blue: 3.0,
        }
        .to_descriptor();
        d.items.push(crate::descriptor::DescriptorItem {
            key: DescriptorKey::new("unknownExtra"),
            value: DescriptorValue::long(9),
        });
        Color::Rgb {
            red: 10.0,
            green: 20.0,
            blue: 30.0,
        }
        .apply_to(&mut d);
        assert_eq!(
            d.items.len(),
            4,
            "the unknown item survives an in-model edit"
        );
        assert_eq!(number(&d, "Grn "), Some(20.0));

        Color::Gray { gray: 5.0 }.apply_to(&mut d);
        assert_eq!(d.class_id.as_str(), "Grsc");
        assert_eq!(d.items.len(), 1, "a model change replaces the descriptor");
    }
}
