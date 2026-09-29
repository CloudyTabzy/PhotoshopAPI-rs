//! Enumerators used by layer effects, as descriptors spell them.
//!
//! Every descriptor `enum` names a type (`BESl`) and a value (`InrB`). Photoshop writes the
//! historical four-character values, and Photoshop 2026 writes long camelCase ones (`innerBevel`)
//! for some; both read, and writing uses the historical value, which every Photoshop version
//! reads. The names are checked against Photoshop-authored files.

/// Compare an ID against a long name ignoring case, spaces and hyphens, so `inner bevel` and
/// `innerBevel` both match.
fn same_name(id: &[u8], long: &str) -> bool {
    let fold = |bytes: &[u8]| -> Vec<u8> {
        bytes
            .iter()
            .filter(|b| !matches!(b, b' ' | b'-' | b'_'))
            .map(u8::to_ascii_lowercase)
            .collect()
    };
    fold(id) == fold(long.as_bytes())
}

macro_rules! id_enum {
    (
        $(#[$meta:meta])*
        $name:ident, $type_id:literal, {
            $($(#[$vmeta:meta])* $variant:ident => ($code:literal, $long:literal)),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The descriptor type ID this enumerator belongs to.
            pub const TYPE_ID: &'static str = $type_id;

            /// The historical value Photoshop writes.
            pub fn code(self) -> &'static str {
                match self {
                    $(Self::$variant => $code),+
                }
            }

            /// Read a value in either spelling; `None` for one this type does not know.
            pub fn from_id(id: &[u8]) -> Option<Self> {
                $(if id == $code.as_bytes() || same_name(id, $long) {
                    return Some(Self::$variant);
                })+
                None
            }
        }
    };
}

id_enum! {
    /// Where a stroke sits relative to the layer's edge (`FStl`).
    StrokePosition, "FStl", {
        Outside => ("OutF", "outside"),
        Center => ("CtrF", "center"),
        Inside => ("InsF", "inside"),
    }
}

id_enum! {
    /// What a stroke is filled with (`FrFl`).
    StrokeFill, "FrFl", {
        Color => ("SClr", "color"),
        Gradient => ("GrFl", "gradient"),
        Pattern => ("Ptrn", "pattern"),
    }
}

id_enum! {
    /// The style of a bevel and emboss (`BESl`).
    BevelStyle, "BESl", {
        InnerBevel => ("InrB", "inner bevel"),
        OuterBevel => ("OtrB", "outer bevel"),
        Emboss => ("Embs", "emboss"),
        PillowEmboss => ("PlEb", "pillow emboss"),
        StrokeEmboss => ("strokeEmboss", "stroke emboss"),
    }
}

id_enum! {
    /// The technique of a bevel and emboss (`bvlT`).
    BevelTechnique, "bvlT", {
        Smooth => ("SfBL", "smooth"),
        ChiselHard => ("PrBL", "chisel hard"),
        ChiselSoft => ("Slmt", "chisel soft"),
    }
}

id_enum! {
    /// The direction of a bevel and emboss (`BESs`).
    BevelDirection, "BESs", {
        Up => ("In  ", "up"),
        Down => ("Out ", "down"),
    }
}

id_enum! {
    /// The technique of a glow (`BETE`).
    GlowTechnique, "BETE", {
        Softer => ("SfBL", "softer"),
        Precise => ("PrBL", "precise"),
    }
}

id_enum! {
    /// Where an inner glow comes from (`IGSr`).
    GlowSource, "IGSr", {
        Edge => ("SrcE", "edge"),
        Center => ("SrcC", "center"),
    }
}

id_enum! {
    /// The shape of a gradient overlay or gradient stroke (`GrdT`).
    GradientStyle, "GrdT", {
        Linear => ("Lnr ", "linear"),
        Radial => ("Rdl ", "radial"),
        Angle => ("Angl", "angle"),
        Reflected => ("Rflc", "reflected"),
        Diamond => ("Dmnd", "diamond"),
        /// Stroke only: a gradient that follows the shape's edge distance.
        ShapeBurst => ("shapeburst", "shape burst"),
    }
}

id_enum! {
    /// How a gradient interpolates between stops (`gradientInterpolationMethodType`).
    GradientInterpolation, "gradientInterpolationMethodType", {
        Perceptual => ("Perc", "perceptual"),
        Linear => ("Lnr ", "linear"),
        Classic => ("Gcls", "classic"),
        Smooth => ("Smoo", "smooth"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_in_the_historical_and_long_spellings() {
        assert_eq!(BevelStyle::from_id(b"InrB"), Some(BevelStyle::InnerBevel));
        assert_eq!(
            BevelStyle::from_id(b"innerBevel"),
            Some(BevelStyle::InnerBevel)
        );
        assert_eq!(
            BevelStyle::from_id(b"inner bevel"),
            Some(BevelStyle::InnerBevel)
        );
        assert_eq!(
            BevelStyle::from_id(b"strokeEmboss"),
            Some(BevelStyle::StrokeEmboss)
        );
        assert_eq!(GradientStyle::from_id(b"Lnr "), Some(GradientStyle::Linear));
        assert_eq!(
            GradientStyle::from_id(b"shapeburst"),
            Some(GradientStyle::ShapeBurst)
        );
        assert_eq!(StrokePosition::from_id(b"nowhere"), None);
    }

    #[test]
    fn writing_uses_the_historical_value_and_every_value_round_trips() {
        assert_eq!(BevelDirection::Up.code(), "In  ");
        for style in [
            BevelStyle::InnerBevel,
            BevelStyle::OuterBevel,
            BevelStyle::Emboss,
            BevelStyle::PillowEmboss,
            BevelStyle::StrokeEmboss,
        ] {
            assert_eq!(BevelStyle::from_id(style.code().as_bytes()), Some(style));
        }
        for interpolation in [
            GradientInterpolation::Perceptual,
            GradientInterpolation::Linear,
            GradientInterpolation::Classic,
            GradientInterpolation::Smooth,
        ] {
            let read = GradientInterpolation::from_id(interpolation.code().as_bytes());
            assert_eq!(read, Some(interpolation));
        }
    }
}
