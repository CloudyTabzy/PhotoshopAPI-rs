//! Typed text-layer enums (`TextLayerEnum.h` upstream).
//!
//! EngineData stores most text enums as plain integers; each such enum here
//! keeps a lossless `Other(i32)` variant so an unrecognized value can be read
//! and written back verbatim. Descriptor-backed enums (warp style/rotation,
//! anti-aliasing) are OSType keys and keep unknown identifiers as bytes.

use psd_core::DescriptorKey;

macro_rules! engine_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident = $raw:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$variant_meta])* $variant, )+
            /// A value this port does not name, kept verbatim.
            Other(i32),
        }

        impl $name {
            /// Map the integer EngineData stores to a variant.
            pub const fn from_raw(raw: i32) -> Self {
                match raw {
                    $( $raw => Self::$variant, )+
                    other => Self::Other(other),
                }
            }

            /// The integer EngineData stores for this variant.
            pub const fn raw(self) -> i32 {
                match self {
                    $( Self::$variant => $raw, )+
                    Self::Other(raw) => raw,
                }
            }
        }
    };
}

engine_enum! {
    /// Text shape (`Children[0]/Cookie/Photoshop/ShapeType`).
    TextShape {
        /// Point text: lines grow from an anchor point.
        Point = 0,
        /// Box (area / paragraph) text bounded by `BoxBounds`.
        Box = 1,
    }
}

engine_enum! {
    /// Writing direction (`EngineDict/Rendered/Shapes/WritingDirection`).
    TextWritingDirection {
        Horizontal = 0,
        Vertical = 2,
    }
}

engine_enum! {
    /// Font technology (`ResourceDict/FontSet[i]/FontType`).
    FontType {
        OpenType = 0,
        TrueType = 1,
    }
}

engine_enum! {
    /// Font script family (`ResourceDict/FontSet[i]/Script`).
    FontScript {
        Roman = 0,
        Cjk = 1,
    }
}

engine_enum! {
    /// Character capitalisation (`FontCaps`).
    FontCaps {
        Normal = 0,
        SmallCaps = 1,
        AllCaps = 2,
    }
}

engine_enum! {
    /// Baseline position (`FontBaseline`).
    FontBaseline {
        Normal = 0,
        Superscript = 1,
        Subscript = 2,
    }
}

engine_enum! {
    /// Bidirectional character direction (`CharacterDirection`).
    CharacterDirection {
        Default = 0,
        LeftToRight = 1,
        RightToLeft = 2,
    }
}

engine_enum! {
    /// Baseline direction (`BaselineDirection`).
    BaselineDirection {
        Default = 0,
        Vertical = 1,
        CrossStream = 2,
    }
}

engine_enum! {
    /// Arabic/Hebrew diacritic positioning (`DiacriticPos`).
    DiacriticPosition {
        OpenType = 0,
        Loose = 1,
        Medium = 2,
        Tight = 3,
    }
}

engine_enum! {
    /// Paragraph alignment (`Justification`).
    Justification {
        Left = 0,
        Right = 1,
        Center = 2,
        JustifyLastLeft = 3,
        JustifyLastRight = 4,
        JustifyLastCenter = 5,
        JustifyAll = 6,
    }
}

engine_enum! {
    /// Leading measurement (`LeadingType`).
    LeadingType {
        BottomToBottom = 0,
        TopToTop = 1,
    }
}

engine_enum! {
    /// Japanese kinsoku processing order (`KinsokuOrder`).
    KinsokuOrder {
        PushInFirst = 0,
        PushOutFirst = 1,
    }
}

/// Keep a four-byte unknown identifier's original length encoding; anything
/// else can only be written with an explicit length.
fn other_key(bytes: &[u8], previous: &DescriptorKey) -> DescriptorKey {
    match <[u8; 4]>::try_from(bytes) {
        Ok(code) if previous.uses_implicit_length() => DescriptorKey::char_id(code),
        _ => DescriptorKey::from_bytes(bytes),
    }
}

/// Photoshop text-warp style (`warp/warpStyle`). Unknown enum identifiers are
/// exposed as their original bytes so callers can inspect them without
/// rewriting the TySh data.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TextWarpStyle {
    NoWarp,
    Arc,
    ArcLower,
    ArcUpper,
    Arch,
    Bulge,
    ShellLower,
    ShellUpper,
    Flag,
    Wave,
    Fish,
    Rise,
    FishEye,
    Inflate,
    Squeeze,
    Twist,
    Custom,
    Other(Vec<u8>),
}

impl TextWarpStyle {
    const NAMED: [(&'static [u8], TextWarpStyle); 17] = [
        (b"warpNone", Self::NoWarp),
        (b"warpArc", Self::Arc),
        (b"warpArcLower", Self::ArcLower),
        (b"warpArcUpper", Self::ArcUpper),
        (b"warpArch", Self::Arch),
        (b"warpBulge", Self::Bulge),
        (b"warpShellLower", Self::ShellLower),
        (b"warpShellUpper", Self::ShellUpper),
        (b"warpFlag", Self::Flag),
        (b"warpWave", Self::Wave),
        (b"warpFish", Self::Fish),
        (b"warpRise", Self::Rise),
        (b"warpFishEye", Self::FishEye),
        (b"warpInflate", Self::Inflate),
        (b"warpSqueeze", Self::Squeeze),
        (b"warpTwist", Self::Twist),
        (b"warpCustom", Self::Custom),
    ];

    pub(crate) fn from_identifier(bytes: &[u8]) -> Self {
        Self::NAMED
            .iter()
            .find(|(name, _)| *name == bytes)
            .map_or_else(|| Self::Other(bytes.to_vec()), |(_, style)| style.clone())
    }

    /// Warp styles are string IDs (`warpArc`); Photoshop writes them with an
    /// explicit key length.
    pub(crate) fn descriptor_key(&self, previous: &DescriptorKey) -> DescriptorKey {
        match self {
            Self::Other(bytes) => other_key(bytes, previous),
            named => {
                let (name, _) = Self::NAMED
                    .iter()
                    .find(|(_, style)| style == named)
                    .expect("every named warp style has an identifier");
                DescriptorKey::from_bytes(*name)
            }
        }
    }
}

/// Orientation of the text-warp effect (`warp/warpRotate`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TextWarpRotation {
    Horizontal,
    Vertical,
    Other(Vec<u8>),
}

impl TextWarpRotation {
    pub(crate) fn from_identifier(bytes: &[u8]) -> Self {
        match bytes {
            b"Hrzn" => Self::Horizontal,
            b"Vrtc" => Self::Vertical,
            other => Self::Other(other.to_vec()),
        }
    }

    /// `Hrzn`/`Vrtc` are Photoshop char IDs (zero-length key encoding).
    pub(crate) fn descriptor_key(&self, previous: &DescriptorKey) -> DescriptorKey {
        match self {
            Self::Horizontal => DescriptorKey::char_id(*b"Hrzn"),
            Self::Vertical => DescriptorKey::char_id(*b"Vrtc"),
            Self::Other(bytes) => other_key(bytes, previous),
        }
    }
}

/// Text anti-aliasing (`TxLr/AntA`, enum type `Annt`).
///
/// Photoshop writes the char IDs `Anno`/`AnCr`/`AnSt`/`AnSm`; Sharp has no
/// char ID and is written as the string ID `antiAliasSharp`. Reads also
/// accept the long string IDs of the other four. Photoshop-internal values
/// that the UI cannot select (`AnLo`/`AnMd`/`AnHi`) surface as `Other`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AntiAliasMethod {
    NoAntiAlias,
    Crisp,
    Strong,
    Smooth,
    Sharp,
    Other(Vec<u8>),
}

impl AntiAliasMethod {
    pub(crate) fn from_identifier(bytes: &[u8]) -> Self {
        match bytes {
            b"Anno" | b"antiAliasNone" => Self::NoAntiAlias,
            b"AnCr" | b"antiAliasCrisp" => Self::Crisp,
            b"AnSt" | b"antiAliasStrong" => Self::Strong,
            b"AnSm" | b"antiAliasSmooth" => Self::Smooth,
            b"antiAliasSharp" => Self::Sharp,
            other => Self::Other(other.to_vec()),
        }
    }

    /// The canonical identifier Photoshop writes for each method.
    pub(crate) fn descriptor_key(&self, previous: &DescriptorKey) -> DescriptorKey {
        match self {
            Self::NoAntiAlias => DescriptorKey::char_id(*b"Anno"),
            Self::Crisp => DescriptorKey::char_id(*b"AnCr"),
            Self::Strong => DescriptorKey::char_id(*b"AnSt"),
            Self::Smooth => DescriptorKey::char_id(*b"AnSm"),
            Self::Sharp => DescriptorKey::from_bytes(*b"antiAliasSharp"),
            Self::Other(bytes) => other_key(bytes, previous),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_enums_round_trip_known_and_unknown_values() {
        assert_eq!(Justification::from_raw(2), Justification::Center);
        assert_eq!(Justification::Center.raw(), 2);
        assert_eq!(Justification::from_raw(42), Justification::Other(42));
        assert_eq!(Justification::Other(42).raw(), 42);
        assert_eq!(
            TextWritingDirection::from_raw(2),
            TextWritingDirection::Vertical
        );
        assert_eq!(
            TextWritingDirection::from_raw(1),
            TextWritingDirection::Other(1)
        );
    }

    #[test]
    fn descriptor_enums_use_photoshop_key_encodings() {
        let explicit = DescriptorKey::new("warpNone");
        let implicit = DescriptorKey::char_id(*b"AnCr");

        let arc = TextWarpStyle::Arc.descriptor_key(&explicit);
        assert_eq!(arc.as_bytes(), b"warpArc");
        assert!(!arc.uses_implicit_length());
        assert_eq!(
            TextWarpStyle::from_identifier(b"warpArc"),
            TextWarpStyle::Arc
        );

        let vertical = TextWarpRotation::Vertical.descriptor_key(&explicit);
        assert!(vertical.uses_implicit_length());

        let sharp = AntiAliasMethod::Sharp.descriptor_key(&implicit);
        assert_eq!(sharp.as_bytes(), b"antiAliasSharp");
        assert!(!sharp.uses_implicit_length());
        let crisp = AntiAliasMethod::Crisp.descriptor_key(&sharp);
        assert!(crisp.uses_implicit_length());
        assert_eq!(
            AntiAliasMethod::from_identifier(b"antiAliasSmooth"),
            AntiAliasMethod::Smooth
        );
        assert_eq!(
            AntiAliasMethod::from_identifier(b"AnHi"),
            AntiAliasMethod::Other(b"AnHi".to_vec())
        );

        // Unknown four-byte identifiers keep the encoding they replace.
        let unknown = TextWarpRotation::Other(b"Zzzz".to_vec());
        assert!(unknown.descriptor_key(&implicit).uses_implicit_length());
        assert!(!unknown.descriptor_key(&explicit).uses_implicit_length());
    }
}
