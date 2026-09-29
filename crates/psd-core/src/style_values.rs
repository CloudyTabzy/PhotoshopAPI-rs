//! Small values shared by layer effects and fills: an offset, a pattern reference and a contour.
//!
//! Each reads from a descriptor with `from_descriptor` (`None` when the shape is not one it
//! models), builds a fresh one with `to_descriptor` in the layout Photoshop writes, and
//! patches an existing one with `apply_to`, which leaves items it does not model alone.

use crate::color::number;
use crate::descriptor::{Descriptor, DescriptorValue};
use crate::descriptor_build::UNIT_PERCENT;

/// Whether a point's coordinates are stored as percentages (`UntF#Prc`) or plain doubles.
///
/// Photoshop uses both: a gradient's `Ofst` and a stroke's `phase` are percentages, a bevel
/// texture's `phase` is plain. The unit is part of the value so that patching keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffsetUnits {
    Percent,
    Plain,
}

/// A `Pnt` descriptor: `Hrzn` and `Vrtc`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Offset {
    pub horizontal: f64,
    pub vertical: f64,
    pub units: OffsetUnits,
}

impl Offset {
    /// A point in percent.
    pub fn percent(horizontal: f64, vertical: f64) -> Self {
        Self {
            horizontal,
            vertical,
            units: OffsetUnits::Percent,
        }
    }

    /// Read a `Pnt` descriptor.
    pub fn from_descriptor(descriptor: &Descriptor) -> Option<Self> {
        let units = match descriptor.get("Hrzn")? {
            DescriptorValue::UnitFloat { unit, .. } if *unit == UNIT_PERCENT => {
                OffsetUnits::Percent
            }
            DescriptorValue::Double(_) => OffsetUnits::Plain,
            _ => return None,
        };
        Some(Self {
            horizontal: number(descriptor, "Hrzn")?,
            vertical: number(descriptor, "Vrtc")?,
            units,
        })
    }

    /// The point as a fresh descriptor.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("Pnt ");
        self.apply_to(&mut descriptor);
        descriptor
    }

    /// Set the coordinates, leaving other items in place.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let value = |v| match self.units {
            OffsetUnits::Percent => DescriptorValue::percent(v),
            OffsetUnits::Plain => DescriptorValue::double(v),
        };
        descriptor.set("Hrzn", value(self.horizontal));
        descriptor.set("Vrtc", value(self.vertical));
    }
}

/// A `Ptrn` descriptor: a pattern's display name and its identifier (a GUID string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternRef {
    pub name: String,
    pub id: String,
}

impl PatternRef {
    /// Read a `Ptrn` descriptor.
    pub fn from_descriptor(descriptor: &Descriptor) -> Option<Self> {
        Some(Self {
            name: descriptor.get_text("Nm  ")?,
            id: descriptor.get_text("Idnt")?,
        })
    }

    /// The reference as a fresh descriptor.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("Ptrn");
        self.apply_to(&mut descriptor);
        descriptor
    }

    /// Set the name and identifier, leaving other items in place.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        descriptor.set_text("Nm  ", &self.name);
        descriptor.set_text("Idnt", &self.id);
    }
}

/// One point of a contour curve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContourPoint {
    /// Input, 0-255.
    pub horizontal: f64,
    /// Output, 0-255.
    pub vertical: f64,
    /// `Cnty`: whether the curve is smooth at this point. `None` when the file does not say,
    /// which is how Photoshop writes its default (linear) contour.
    pub smooth: Option<bool>,
}

/// A `ShpC` descriptor: the transfer curve of a shadow, glow, bevel or satin.
#[derive(Debug, Clone, PartialEq)]
pub struct Contour {
    pub name: String,
    pub points: Vec<ContourPoint>,
}

impl Contour {
    /// Photoshop's default contour: a straight line from (0, 0) to (255, 255), named
    /// `Linear`, with no smoothness flags.
    pub fn linear() -> Self {
        Self {
            name: "Linear".to_owned(),
            points: vec![
                ContourPoint {
                    horizontal: 0.0,
                    vertical: 0.0,
                    smooth: None,
                },
                ContourPoint {
                    horizontal: 255.0,
                    vertical: 255.0,
                    smooth: None,
                },
            ],
        }
    }

    /// Read a `ShpC` descriptor. A point without both coordinates makes the whole contour
    /// unmodelled (`None`) rather than silently shorter.
    pub fn from_descriptor(descriptor: &Descriptor) -> Option<Self> {
        let curve = descriptor.get("Crv ")?.as_list()?;
        let points = curve
            .iter()
            .map(|value| {
                let point = value.as_descriptor()?;
                Some(ContourPoint {
                    horizontal: number(point, "Hrzn")?,
                    vertical: number(point, "Vrtc")?,
                    smooth: point.get_bool("Cnty"),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            name: descriptor.get_text("Nm  ")?,
            points,
        })
    }

    /// The contour as a fresh descriptor.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("ShpC");
        self.apply_to(&mut descriptor);
        descriptor
    }

    /// Set the name and curve. The curve is rebuilt point by point: a point that already
    /// exists at the same position keeps items this type does not model.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        descriptor.set_text("Nm  ", &self.name);
        let existing: &[DescriptorValue] = descriptor
            .get("Crv ")
            .and_then(DescriptorValue::as_list)
            .unwrap_or_default();
        let mut curve = Vec::with_capacity(self.points.len());
        for (index, point) in self.points.iter().enumerate() {
            let mut item = match existing.get(index) {
                Some(DescriptorValue::Descriptor(d)) => d.clone(),
                _ => Descriptor::with_class("CrPt"),
            };
            item.set("Hrzn", DescriptorValue::double(point.horizontal));
            item.set("Vrtc", DescriptorValue::double(point.vertical));
            match point.smooth {
                Some(smooth) => item.set("Cnty", DescriptorValue::boolean(smooth)),
                None => {
                    item.remove("Cnty");
                }
            }
            curve.push(DescriptorValue::Descriptor(item));
        }
        descriptor.set("Crv ", DescriptorValue::List(curve));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_keeps_its_units_through_a_round_trip_and_a_patch() {
        let percent = Offset::percent(12.5, -3.0);
        assert_eq!(
            Offset::from_descriptor(&percent.to_descriptor()),
            Some(percent)
        );
        let plain = Offset {
            horizontal: 1.0,
            vertical: 2.0,
            units: OffsetUnits::Plain,
        };
        assert_eq!(Offset::from_descriptor(&plain.to_descriptor()), Some(plain));

        let mut d = percent.to_descriptor();
        Offset::percent(1.0, 2.0).apply_to(&mut d);
        assert_eq!(d.get_unit("Hrzn", *b"#Prc"), Some(1.0));
    }

    #[test]
    fn a_pattern_reference_round_trips_and_ignores_a_terminating_null() {
        let pattern = PatternRef {
            name: "Paper".to_owned(),
            id: "guid-1".to_owned(),
        };
        let mut d = pattern.to_descriptor();
        assert_eq!(PatternRef::from_descriptor(&d), Some(pattern.clone()));
        // Photoshop writes strings with a terminating null; it is not part of the value.
        d.set("Nm  ", DescriptorValue::text("Paper\0"));
        assert_eq!(PatternRef::from_descriptor(&d), Some(pattern));
    }

    #[test]
    fn the_default_contour_is_a_two_point_line_without_smoothness_flags() {
        let d = Contour::linear().to_descriptor();
        assert_eq!(Contour::from_descriptor(&d), Some(Contour::linear()));
        let curve = d.get("Crv ").unwrap().as_list().unwrap();
        assert_eq!(curve.len(), 2);
        assert!(curve
            .iter()
            .all(|p| !p.as_descriptor().unwrap().contains("Cnty")));
    }

    #[test]
    fn a_custom_contour_records_smoothness_on_every_point() {
        let contour = Contour {
            name: "Custom".to_owned(),
            points: vec![
                ContourPoint {
                    horizontal: 0.0,
                    vertical: 0.0,
                    smooth: Some(true),
                },
                ContourPoint {
                    horizontal: 96.0,
                    vertical: 200.0,
                    smooth: Some(false),
                },
                ContourPoint {
                    horizontal: 255.0,
                    vertical: 255.0,
                    smooth: Some(true),
                },
            ],
        };
        assert_eq!(
            Contour::from_descriptor(&contour.to_descriptor()),
            Some(contour)
        );
    }

    #[test]
    fn patching_a_contour_keeps_unknown_items_on_surviving_points() {
        let mut d = Contour::linear().to_descriptor();
        // Give the first point an item this type does not model.
        let DescriptorValue::List(list) = d.get_mut("Crv ").unwrap() else {
            unreachable!()
        };
        let DescriptorValue::Descriptor(first) = &mut list[0] else {
            unreachable!()
        };
        first.set("extra", DescriptorValue::long(7));

        let mut wider = Contour::linear();
        wider.points.insert(
            1,
            ContourPoint {
                horizontal: 128.0,
                vertical: 64.0,
                smooth: None,
            },
        );
        wider.apply_to(&mut d);

        let read = Contour::from_descriptor(&d).unwrap();
        assert_eq!(read, wider);
        let list = d.get("Crv ").unwrap().as_list().unwrap();
        assert!(list[0].as_descriptor().unwrap().contains("extra"));
    }

    #[test]
    fn a_contour_with_an_incomplete_point_is_not_modelled() {
        let mut d = Contour::linear().to_descriptor();
        let DescriptorValue::List(list) = d.get_mut("Crv ").unwrap() else {
            unreachable!()
        };
        let DescriptorValue::Descriptor(point) = &mut list[1] else {
            unreachable!()
        };
        point.remove("Vrtc");
        assert_eq!(Contour::from_descriptor(&d), None);
    }
}
