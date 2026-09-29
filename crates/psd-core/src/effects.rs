//! Typed models of the layer effects stored in `lfx2`, `lmfx` and `lfxs` descriptors.
//!
//! Each effect is a struct of `Option` fields that mirrors what its descriptor holds: reading
//! gives `Some` for a field the file has and `None` for one it lacks (or holds in a shape this
//! model does not read), and writing changes a descriptor only where a field is `Some` and
//! differs from what the descriptor already says. So an edit touches what it names, an edit
//! that changes nothing changes no bytes, and everything the model does not know (unknown
//! items, their order, their key encodings, the spelling of a value that is unchanged) stays
//! exactly as it was.
//!
//! Building a descriptor from scratch uses the item order Photoshop-authored files show, which
//! is not the order the public `ag-psd-rs` reference (<https://github.com/Vasyanator/ag-psd-rs>)
//! writes: the tables below were counted from real files, and a `Default` effect carries every
//! field so that a new effect has the full layout Photoshop writes (a gradient overlay in
//! particular is reset to Normal by Photoshop unless it has the complete one).
//!
//! Percentages are the numbers the file holds (0-100), lengths are in pixels and angles in
//! degrees. `Ckmt` ("choke" or "spread") is stored with a pixel unit but holds a percentage;
//! it is read and written as stored.

use crate::color::{number, Color};
use crate::descriptor::{Descriptor, DescriptorValue};
use crate::descriptor_build::{UNIT_ANGLE, UNIT_PERCENT, UNIT_PIXELS};
use crate::effect_enums::{
    BevelDirection, BevelStyle, BevelTechnique, GlowSource, GlowTechnique, GradientInterpolation,
    GradientStyle, StrokeFill, StrokePosition,
};
use crate::enums::BlendMode;
use crate::gradient::Gradient;
use crate::style_values::{Contour, Offset, PatternRef};

// ---------------------------------------------------------------------------------------
// Item order, as Photoshop writes it (counted from real files)
// ---------------------------------------------------------------------------------------

const DROP_SHADOW_ORDER: [&str; 15] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Clr ",
    "Opct",
    "uglg",
    "lagl",
    "Dstn",
    "Ckmt",
    "blur",
    "Nose",
    "AntA",
    "TrnS",
    "layerConceals",
];
const INNER_SHADOW_ORDER: [&str; 14] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Clr ",
    "Opct",
    "uglg",
    "lagl",
    "Dstn",
    "Ckmt",
    "blur",
    "Nose",
    "AntA",
    "TrnS",
];
const OUTER_GLOW_ORDER: [&str; 15] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Clr ",
    "Grad",
    "Opct",
    "GlwT",
    "Ckmt",
    "blur",
    "Nose",
    "ShdN",
    "AntA",
    "TrnS",
    "Inpr",
];
const INNER_GLOW_ORDER: [&str; 16] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Clr ",
    "Grad",
    "Opct",
    "GlwT",
    "Ckmt",
    "blur",
    "Nose",
    "ShdN",
    "AntA",
    "TrnS",
    "Inpr",
    "glwS",
];
const BEVEL_ORDER: [&str; 31] = [
    "enab",
    "present",
    "showInDialog",
    "hglM",
    "hglC",
    "hglO",
    "sdwM",
    "sdwC",
    "sdwO",
    "bvlT",
    "bvlS",
    "uglg",
    "lagl",
    "Lald",
    "srgR",
    "blur",
    "bvlD",
    "TrnS",
    "antialiasGloss",
    "Sftn",
    "useShape",
    "MpgS",
    "AntA",
    "Inpr",
    "useTexture",
    "InvT",
    "Algn",
    "Scl ",
    "textureDepth",
    "Ptrn",
    "phase",
];
const COLOR_OVERLAY_ORDER: [&str; 6] = ["enab", "present", "showInDialog", "Md  ", "Clr ", "Opct"];
const SATIN_ORDER: [&str; 12] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Clr ",
    "AntA",
    "Invr",
    "Opct",
    "lagl",
    "Dstn",
    "blur",
    "MpgS",
];
const GRADIENT_OVERLAY_ORDER: [&str; 14] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Opct",
    "Grad",
    "Angl",
    "Type",
    "Rvrs",
    "Dthr",
    "gs99",
    "Algn",
    "Scl ",
    "Ofst",
];
const PATTERN_OVERLAY_ORDER: [&str; 10] = [
    "enab",
    "present",
    "showInDialog",
    "Md  ",
    "Opct",
    "Ptrn",
    "Angl",
    "Scl ",
    "Algn",
    "phase",
];
const STROKE_ORDER: [&str; 22] = [
    "enab",
    "present",
    "showInDialog",
    "Styl",
    "PntT",
    "Md  ",
    "Opct",
    "Sz  ",
    "Clr ",
    "Grad",
    "gradientsInterpolationMethod",
    "Angl",
    "Type",
    "Rvrs",
    "Dthr",
    "Ptrn",
    "Scl ",
    "Algn",
    "Lnkd",
    "Ofst",
    "phase",
    "overprint",
];

// ---------------------------------------------------------------------------------------
// Reading and patching fields
// ---------------------------------------------------------------------------------------

/// The common shape of the enumerators in [`effect_enums`](crate::effect_enums).
trait Choice: Copy + PartialEq {
    const TYPE_ID: &'static str;
    fn code(self) -> &'static str;
    fn from_id(id: &[u8]) -> Option<Self>;
}

macro_rules! choices {
    ($($t:ty),+) => {$(
        impl Choice for $t {
            const TYPE_ID: &'static str = <$t>::TYPE_ID;
            fn code(self) -> &'static str { <$t>::code(self) }
            fn from_id(id: &[u8]) -> Option<Self> { <$t>::from_id(id) }
        }
    )+};
}
choices!(
    BevelDirection,
    BevelStyle,
    BevelTechnique,
    GlowSource,
    GlowTechnique,
    GradientInterpolation,
    GradientStyle,
    StrokeFill,
    StrokePosition
);

/// Read-side helpers over one effect descriptor.
struct Read<'a>(&'a Descriptor);

impl Read<'_> {
    fn flag(&self, key: &str) -> Option<bool> {
        self.0.get_bool(key)
    }
    fn number(&self, key: &str) -> Option<f64> {
        number(self.0, key)
    }
    fn choice<E: Choice>(&self, key: &str) -> Option<E> {
        E::from_id(self.0.get_enum(key)?)
    }
    fn blend(&self, key: &str) -> Option<BlendMode> {
        BlendMode::from_descriptor_enum(self.0.get_enum(key)?)
    }
    fn child(&self, key: &str) -> Option<&Descriptor> {
        self.0.get(key)?.as_descriptor()
    }
    fn color(&self, key: &str) -> Option<Color> {
        Color::from_descriptor(self.child(key)?)
    }
    fn contour(&self, key: &str) -> Option<Contour> {
        Contour::from_descriptor(self.child(key)?)
    }
    fn gradient(&self, key: &str) -> Option<Gradient> {
        Gradient::from_descriptor(self.child(key)?)
    }
    fn pattern(&self, key: &str) -> Option<PatternRef> {
        PatternRef::from_descriptor(self.child(key)?)
    }
    fn offset(&self, key: &str) -> Option<Offset> {
        Offset::from_descriptor(self.child(key)?)
    }
}

/// Write-side helpers: every method writes only a `Some` field that differs from what the
/// descriptor holds, placing a new item by the effect's order table.
struct Patch<'a> {
    d: &'a mut Descriptor,
    order: &'static [&'static str],
}

impl Patch<'_> {
    fn flag(&mut self, key: &str, value: Option<bool>) {
        if let Some(value) = value {
            if self.d.get_bool(key) != Some(value) {
                self.d
                    .set_ordered(key, DescriptorValue::boolean(value), self.order);
            }
        }
    }

    fn unit(&mut self, key: &str, unit: [u8; 4], value: Option<f64>) {
        if let Some(value) = value {
            if number(self.d, key) != Some(value) {
                self.d
                    .set_ordered(key, DescriptorValue::unit(unit, value), self.order);
            }
        }
    }

    fn percent(&mut self, key: &str, value: Option<f64>) {
        self.unit(key, UNIT_PERCENT, value);
    }

    fn pixels(&mut self, key: &str, value: Option<f64>) {
        self.unit(key, UNIT_PIXELS, value);
    }

    fn angle(&mut self, key: &str, value: Option<f64>) {
        self.unit(key, UNIT_ANGLE, value);
    }

    fn choice<E: Choice>(&mut self, key: &str, value: Option<E>) {
        if let Some(value) = value {
            let current = self.d.get_enum(key).and_then(E::from_id);
            if current != Some(value) {
                let entry = DescriptorValue::enumerated(E::TYPE_ID, value.code());
                self.d.set_ordered(key, entry, self.order);
            }
        }
    }

    fn blend(&mut self, key: &str, value: Option<BlendMode>) {
        let Some(value) = value else { return };
        let current = self
            .d
            .get_enum(key)
            .and_then(BlendMode::from_descriptor_enum);
        if current == Some(value) {
            return;
        }
        if let Some(spelling) = value.to_descriptor_enum() {
            let entry = DescriptorValue::Enumerated {
                type_id: crate::descriptor::DescriptorKey::id("BlnM"),
                value: spelling,
            };
            self.d.set_ordered(key, entry, self.order);
        }
    }

    /// Patch the nested descriptor at `key` with `apply`, or build a fresh one with `build`
    /// when there is none.
    fn nested(
        &mut self,
        key: &str,
        apply: impl FnOnce(&mut Descriptor),
        build: impl FnOnce() -> Descriptor,
    ) {
        if let Some(DescriptorValue::Descriptor(child)) = self.d.get_mut(key) {
            apply(child);
        } else {
            self.d
                .set_ordered(key, DescriptorValue::Descriptor(build()), self.order);
        }
    }

    fn color(&mut self, key: &str, value: &Option<Color>) {
        if let Some(value) = value {
            self.nested(key, |d| value.apply_to(d), || value.to_descriptor());
        }
    }

    fn contour(&mut self, key: &str, value: &Option<Contour>) {
        if let Some(value) = value {
            self.nested(key, |d| value.apply_to(d), || value.to_descriptor());
        }
    }

    fn gradient(&mut self, key: &str, value: &Option<Gradient>) {
        if let Some(value) = value {
            self.nested(key, |d| value.apply_to(d), || value.to_descriptor());
        }
    }

    fn pattern(&mut self, key: &str, value: &Option<PatternRef>) {
        if let Some(value) = value {
            self.nested(key, |d| value.apply_to(d), || value.to_descriptor());
        }
    }

    fn offset(&mut self, key: &str, value: &Option<Offset>) {
        if let Some(value) = value {
            self.nested(key, |d| value.apply_to(d), || value.to_descriptor());
        }
    }
}

// ---------------------------------------------------------------------------------------
// Colours and small defaults
// ---------------------------------------------------------------------------------------

fn rgb(red: f64, green: f64, blue: f64) -> Option<Color> {
    Some(Color::Rgb { red, green, blue })
}

/// The colour Photoshop starts a glow from.
const GLOW_YELLOW: (f64, f64, f64) = (255.0, 255.0, 190.0);

// ---------------------------------------------------------------------------------------
// Shadows
// ---------------------------------------------------------------------------------------

/// Which shadow a [`Shadow`] describes; they differ in layout by one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowKind {
    /// `DrSh`, and the entries of `dropShadowMulti`.
    Drop,
    /// `IrSh`, and the entries of `innerShadowMulti`.
    Inner,
}

impl ShadowKind {
    fn order(self) -> &'static [&'static str] {
        match self {
            Self::Drop => &DROP_SHADOW_ORDER,
            Self::Inner => &INNER_SHADOW_ORDER,
        }
    }

    /// The class ID Photoshop gives the effect's descriptor, which is its root key.
    fn class(self) -> &'static str {
        match self {
            Self::Drop => "DrSh",
            Self::Inner => "IrSh",
        }
    }
}

/// A drop shadow or an inner shadow.
#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    pub enabled: Option<bool>,
    /// `present`: absent from files older than Photoshop CS6.
    pub present: Option<bool>,
    /// `showInDialog`: likewise.
    pub show_in_dialog: Option<bool>,
    pub blend_mode: Option<BlendMode>,
    pub color: Option<Color>,
    /// Percent.
    pub opacity: Option<f64>,
    /// `uglg`: use the document's global light angle.
    pub use_global_light: Option<bool>,
    /// Degrees.
    pub angle: Option<f64>,
    /// Pixels.
    pub distance: Option<f64>,
    /// `Ckmt`: spread (drop shadow) or choke (inner shadow), as stored.
    pub spread: Option<f64>,
    /// `blur`: pixels.
    pub size: Option<f64>,
    /// Percent.
    pub noise: Option<f64>,
    pub anti_aliased: Option<bool>,
    /// `TrnS`.
    pub contour: Option<Contour>,
    /// `layerConceals`, drop shadow only: the layer knocks out the shadow.
    pub layer_knocks_out: Option<bool>,
}

impl Shadow {
    /// Photoshop's default shadow of `kind`: multiplied black at 75%, 5 px away at 120 degrees.
    pub fn new(kind: ShadowKind) -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            blend_mode: Some(BlendMode::MULTIPLY),
            color: rgb(0.0, 0.0, 0.0),
            opacity: Some(75.0),
            use_global_light: Some(true),
            angle: Some(120.0),
            distance: Some(5.0),
            spread: Some(0.0),
            size: Some(5.0),
            noise: Some(0.0),
            anti_aliased: Some(false),
            contour: Some(Contour::linear()),
            layer_knocks_out: (kind == ShadowKind::Drop).then_some(true),
        }
    }

    /// Read the fields a shadow descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            blend_mode: r.blend("Md  "),
            color: r.color("Clr "),
            opacity: r.number("Opct"),
            use_global_light: r.flag("uglg"),
            angle: r.number("lagl"),
            distance: r.number("Dstn"),
            spread: r.number("Ckmt"),
            size: r.number("blur"),
            noise: r.number("Nose"),
            anti_aliased: r.flag("AntA"),
            contour: r.contour("TrnS"),
            layer_knocks_out: r.flag("layerConceals"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor, kind: ShadowKind) {
        let mut p = Patch {
            d: descriptor,
            order: kind.order(),
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("Md  ", self.blend_mode);
        p.color("Clr ", &self.color);
        p.percent("Opct", self.opacity);
        p.flag("uglg", self.use_global_light);
        p.angle("lagl", self.angle);
        p.pixels("Dstn", self.distance);
        p.pixels("Ckmt", self.spread);
        p.pixels("blur", self.size);
        p.percent("Nose", self.noise);
        p.flag("AntA", self.anti_aliased);
        p.contour("TrnS", &self.contour);
        if kind == ShadowKind::Drop {
            p.flag("layerConceals", self.layer_knocks_out);
        }
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self, kind: ShadowKind) -> Descriptor {
        let mut descriptor = Descriptor::with_class(kind.class());
        self.apply_to(&mut descriptor, kind);
        descriptor
    }
}

// ---------------------------------------------------------------------------------------
// Glows
// ---------------------------------------------------------------------------------------

/// Which glow a [`Glow`] describes; an inner glow has one more item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlowKind {
    /// `OrGl`.
    Outer,
    /// `IrGl`.
    Inner,
}

impl GlowKind {
    fn order(self) -> &'static [&'static str] {
        match self {
            Self::Outer => &OUTER_GLOW_ORDER,
            Self::Inner => &INNER_GLOW_ORDER,
        }
    }

    /// The class ID Photoshop gives the effect's descriptor, which is its root key.
    fn class(self) -> &'static str {
        match self {
            Self::Outer => "OrGl",
            Self::Inner => "IrGl",
        }
    }
}

/// An outer glow or an inner glow.
#[derive(Debug, Clone, PartialEq)]
pub struct Glow {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    pub blend_mode: Option<BlendMode>,
    /// `Clr `: a solid glow. A gradient glow has `gradient` instead.
    pub color: Option<Color>,
    /// `Grad`.
    pub gradient: Option<Gradient>,
    /// Percent.
    pub opacity: Option<f64>,
    /// `GlwT`.
    pub technique: Option<GlowTechnique>,
    /// `Ckmt`: spread (outer glow) or choke (inner glow), as stored.
    pub spread: Option<f64>,
    /// `blur`: pixels.
    pub size: Option<f64>,
    /// Percent.
    pub noise: Option<f64>,
    /// `ShdN`: jitter, percent.
    pub jitter: Option<f64>,
    pub anti_aliased: Option<bool>,
    /// `TrnS`.
    pub contour: Option<Contour>,
    /// `Inpr`: range, percent.
    pub range: Option<f64>,
    /// `glwS`, inner glow only.
    pub source: Option<GlowSource>,
}

impl Glow {
    /// Photoshop's default glow of `kind`: screened pale yellow at 75%.
    pub fn new(kind: GlowKind) -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            blend_mode: Some(BlendMode::SCREEN),
            color: rgb(GLOW_YELLOW.0, GLOW_YELLOW.1, GLOW_YELLOW.2),
            gradient: None,
            opacity: Some(75.0),
            technique: Some(GlowTechnique::Softer),
            spread: Some(0.0),
            size: Some(5.0),
            noise: Some(0.0),
            jitter: Some(0.0),
            anti_aliased: Some(false),
            contour: Some(Contour::linear()),
            range: Some(50.0),
            source: (kind == GlowKind::Inner).then_some(GlowSource::Edge),
        }
    }

    /// Read the fields a glow descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            blend_mode: r.blend("Md  "),
            color: r.color("Clr "),
            gradient: r.gradient("Grad"),
            opacity: r.number("Opct"),
            technique: r.choice("GlwT"),
            spread: r.number("Ckmt"),
            size: r.number("blur"),
            noise: r.number("Nose"),
            jitter: r.number("ShdN"),
            anti_aliased: r.flag("AntA"),
            contour: r.contour("TrnS"),
            range: r.number("Inpr"),
            source: r.choice("glwS"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor, kind: GlowKind) {
        let mut p = Patch {
            d: descriptor,
            order: kind.order(),
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("Md  ", self.blend_mode);
        p.color("Clr ", &self.color);
        p.gradient("Grad", &self.gradient);
        p.percent("Opct", self.opacity);
        p.choice("GlwT", self.technique);
        p.pixels("Ckmt", self.spread);
        p.pixels("blur", self.size);
        p.percent("Nose", self.noise);
        p.percent("ShdN", self.jitter);
        p.flag("AntA", self.anti_aliased);
        p.contour("TrnS", &self.contour);
        p.percent("Inpr", self.range);
        if kind == GlowKind::Inner {
            p.choice("glwS", self.source);
        }
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self, kind: GlowKind) -> Descriptor {
        let mut descriptor = Descriptor::with_class(kind.class());
        self.apply_to(&mut descriptor, kind);
        descriptor
    }
}

// ---------------------------------------------------------------------------------------
// Bevel and emboss
// ---------------------------------------------------------------------------------------

/// Bevel and emboss (`ebbl`).
#[derive(Debug, Clone, PartialEq)]
pub struct Bevel {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    /// `hglM`, `hglC`, `hglO`: the highlight's blend mode, colour and opacity (percent).
    pub highlight_blend_mode: Option<BlendMode>,
    pub highlight_color: Option<Color>,
    pub highlight_opacity: Option<f64>,
    /// `sdwM`, `sdwC`, `sdwO`: the shadow's blend mode, colour and opacity (percent).
    pub shadow_blend_mode: Option<BlendMode>,
    pub shadow_color: Option<Color>,
    pub shadow_opacity: Option<f64>,
    /// `bvlT`.
    pub technique: Option<BevelTechnique>,
    /// `bvlS`.
    pub style: Option<BevelStyle>,
    /// `uglg`.
    pub use_global_light: Option<bool>,
    /// `lagl`: degrees.
    pub angle: Option<f64>,
    /// `Lald`: the light's altitude, degrees.
    pub altitude: Option<f64>,
    /// `srgR`: depth, percent.
    pub depth: Option<f64>,
    /// `blur`: pixels.
    pub size: Option<f64>,
    /// `bvlD`.
    pub direction: Option<BevelDirection>,
    /// `TrnS`: the gloss contour.
    pub gloss_contour: Option<Contour>,
    /// `antialiasGloss`.
    pub anti_alias_gloss: Option<bool>,
    /// `Sftn`: soften, pixels.
    pub soften: Option<f64>,
    /// `useShape`: the Contour option, on.
    pub use_shape: Option<bool>,
    /// `MpgS`: the contour of the Contour option.
    pub contour: Option<Contour>,
    /// `AntA`: anti-aliasing of that contour.
    pub contour_anti_aliased: Option<bool>,
    /// `Inpr`: its range, percent.
    pub contour_range: Option<f64>,
    /// `useTexture`: the Texture option, on.
    pub use_texture: Option<bool>,
    /// `InvT`.
    pub texture_invert: Option<bool>,
    /// `Algn`: link the texture with the layer.
    pub texture_linked: Option<bool>,
    /// `Scl `: texture scale, percent.
    pub texture_scale: Option<f64>,
    /// `textureDepth`: percent.
    pub texture_depth: Option<f64>,
    /// `Ptrn`.
    pub texture_pattern: Option<PatternRef>,
    /// `phase`.
    pub texture_phase: Option<Offset>,
}

impl Default for Bevel {
    /// Photoshop's default inner bevel: smooth, 100% deep, 5 px, lit from 120 degrees at 30.
    fn default() -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            highlight_blend_mode: Some(BlendMode::SCREEN),
            highlight_color: rgb(255.0, 255.0, 255.0),
            highlight_opacity: Some(75.0),
            shadow_blend_mode: Some(BlendMode::MULTIPLY),
            shadow_color: rgb(0.0, 0.0, 0.0),
            shadow_opacity: Some(75.0),
            technique: Some(BevelTechnique::Smooth),
            style: Some(BevelStyle::InnerBevel),
            use_global_light: Some(true),
            angle: Some(120.0),
            altitude: Some(30.0),
            depth: Some(100.0),
            size: Some(5.0),
            direction: Some(BevelDirection::Up),
            gloss_contour: Some(Contour::linear()),
            anti_alias_gloss: Some(false),
            soften: Some(0.0),
            use_shape: Some(false),
            contour: None,
            contour_anti_aliased: None,
            contour_range: None,
            use_texture: Some(false),
            texture_invert: None,
            texture_linked: None,
            texture_scale: None,
            texture_depth: None,
            texture_pattern: None,
            texture_phase: None,
        }
    }
}

impl Bevel {
    /// Read the fields a bevel descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            highlight_blend_mode: r.blend("hglM"),
            highlight_color: r.color("hglC"),
            highlight_opacity: r.number("hglO"),
            shadow_blend_mode: r.blend("sdwM"),
            shadow_color: r.color("sdwC"),
            shadow_opacity: r.number("sdwO"),
            technique: r.choice("bvlT"),
            style: r.choice("bvlS"),
            use_global_light: r.flag("uglg"),
            angle: r.number("lagl"),
            altitude: r.number("Lald"),
            depth: r.number("srgR"),
            size: r.number("blur"),
            direction: r.choice("bvlD"),
            gloss_contour: r.contour("TrnS"),
            anti_alias_gloss: r.flag("antialiasGloss"),
            soften: r.number("Sftn"),
            use_shape: r.flag("useShape"),
            contour: r.contour("MpgS"),
            contour_anti_aliased: r.flag("AntA"),
            contour_range: r.number("Inpr"),
            use_texture: r.flag("useTexture"),
            texture_invert: r.flag("InvT"),
            texture_linked: r.flag("Algn"),
            texture_scale: r.number("Scl "),
            texture_depth: r.number("textureDepth"),
            texture_pattern: r.pattern("Ptrn"),
            texture_phase: r.offset("phase"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let mut p = Patch {
            d: descriptor,
            order: &BEVEL_ORDER,
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("hglM", self.highlight_blend_mode);
        p.color("hglC", &self.highlight_color);
        p.percent("hglO", self.highlight_opacity);
        p.blend("sdwM", self.shadow_blend_mode);
        p.color("sdwC", &self.shadow_color);
        p.percent("sdwO", self.shadow_opacity);
        p.choice("bvlT", self.technique);
        p.choice("bvlS", self.style);
        p.flag("uglg", self.use_global_light);
        p.angle("lagl", self.angle);
        p.angle("Lald", self.altitude);
        p.percent("srgR", self.depth);
        p.pixels("blur", self.size);
        p.choice("bvlD", self.direction);
        p.contour("TrnS", &self.gloss_contour);
        p.flag("antialiasGloss", self.anti_alias_gloss);
        p.pixels("Sftn", self.soften);
        p.flag("useShape", self.use_shape);
        p.contour("MpgS", &self.contour);
        p.flag("AntA", self.contour_anti_aliased);
        p.percent("Inpr", self.contour_range);
        p.flag("useTexture", self.use_texture);
        p.flag("InvT", self.texture_invert);
        p.flag("Algn", self.texture_linked);
        p.percent("Scl ", self.texture_scale);
        p.percent("textureDepth", self.texture_depth);
        p.pattern("Ptrn", &self.texture_pattern);
        p.offset("phase", &self.texture_phase);
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("ebbl");
        self.apply_to(&mut descriptor);
        descriptor
    }
}

// ---------------------------------------------------------------------------------------
// Overlays, satin and stroke
// ---------------------------------------------------------------------------------------

/// A colour overlay (`SoFi`).
#[derive(Debug, Clone, PartialEq)]
pub struct ColorOverlay {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    pub blend_mode: Option<BlendMode>,
    pub color: Option<Color>,
    /// Percent.
    pub opacity: Option<f64>,
}

impl Default for ColorOverlay {
    /// Photoshop's default colour overlay: normal red at 100%.
    fn default() -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            blend_mode: Some(BlendMode::NORMAL),
            color: rgb(255.0, 0.0, 0.0),
            opacity: Some(100.0),
        }
    }
}

impl ColorOverlay {
    /// Read the fields a colour-overlay descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            blend_mode: r.blend("Md  "),
            color: r.color("Clr "),
            opacity: r.number("Opct"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let mut p = Patch {
            d: descriptor,
            order: &COLOR_OVERLAY_ORDER,
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("Md  ", self.blend_mode);
        p.color("Clr ", &self.color);
        p.percent("Opct", self.opacity);
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("SoFi");
        self.apply_to(&mut descriptor);
        descriptor
    }
}

/// Satin (`ChFX`).
#[derive(Debug, Clone, PartialEq)]
pub struct Satin {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    pub blend_mode: Option<BlendMode>,
    pub color: Option<Color>,
    pub anti_aliased: Option<bool>,
    /// `Invr`.
    pub invert: Option<bool>,
    /// Percent.
    pub opacity: Option<f64>,
    /// `lagl`: degrees.
    pub angle: Option<f64>,
    /// `Dstn`: pixels.
    pub distance: Option<f64>,
    /// `blur`: pixels.
    pub size: Option<f64>,
    /// `MpgS`.
    pub contour: Option<Contour>,
}

impl Default for Satin {
    /// Photoshop's default satin: multiplied black at 50%, inverted, 11 px at 19 degrees.
    fn default() -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            blend_mode: Some(BlendMode::MULTIPLY),
            color: rgb(0.0, 0.0, 0.0),
            anti_aliased: Some(true),
            invert: Some(true),
            opacity: Some(50.0),
            angle: Some(19.0),
            distance: Some(11.0),
            size: Some(14.0),
            contour: Some(Contour::linear()),
        }
    }
}

impl Satin {
    /// Read the fields a satin descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            blend_mode: r.blend("Md  "),
            color: r.color("Clr "),
            anti_aliased: r.flag("AntA"),
            invert: r.flag("Invr"),
            opacity: r.number("Opct"),
            angle: r.number("lagl"),
            distance: r.number("Dstn"),
            size: r.number("blur"),
            contour: r.contour("MpgS"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let mut p = Patch {
            d: descriptor,
            order: &SATIN_ORDER,
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("Md  ", self.blend_mode);
        p.color("Clr ", &self.color);
        p.flag("AntA", self.anti_aliased);
        p.flag("Invr", self.invert);
        p.percent("Opct", self.opacity);
        p.angle("lagl", self.angle);
        p.pixels("Dstn", self.distance);
        p.pixels("blur", self.size);
        p.contour("MpgS", &self.contour);
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("ChFX");
        self.apply_to(&mut descriptor);
        descriptor
    }
}

/// A gradient overlay (`GrFl`, and the entries of `gradientFillMulti`).
#[derive(Debug, Clone, PartialEq)]
pub struct GradientOverlay {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    pub blend_mode: Option<BlendMode>,
    /// Percent.
    pub opacity: Option<f64>,
    /// `Grad`.
    pub gradient: Option<Gradient>,
    /// `Angl`: degrees.
    pub angle: Option<f64>,
    /// `Type`.
    pub style: Option<GradientStyle>,
    /// `Rvrs`.
    pub reverse: Option<bool>,
    /// `Dthr`.
    pub dither: Option<bool>,
    /// `gs99`.
    pub interpolation: Option<GradientInterpolation>,
    /// `Algn`: align with the layer.
    pub align: Option<bool>,
    /// `Scl `: percent.
    pub scale: Option<f64>,
    /// `Ofst`.
    pub offset: Option<Offset>,
}

impl Default for GradientOverlay {
    /// Photoshop's default gradient overlay: normal black-to-white, linear at 90 degrees.
    fn default() -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            blend_mode: Some(BlendMode::NORMAL),
            opacity: Some(100.0),
            gradient: Some(Gradient::black_to_white()),
            angle: Some(90.0),
            style: Some(GradientStyle::Linear),
            reverse: Some(false),
            dither: Some(false),
            interpolation: Some(GradientInterpolation::Classic),
            align: Some(true),
            scale: Some(100.0),
            offset: Some(Offset::percent(0.0, 0.0)),
        }
    }
}

impl GradientOverlay {
    /// Read the fields a gradient-overlay descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            blend_mode: r.blend("Md  "),
            opacity: r.number("Opct"),
            gradient: r.gradient("Grad"),
            angle: r.number("Angl"),
            style: r.choice("Type"),
            reverse: r.flag("Rvrs"),
            dither: r.flag("Dthr"),
            interpolation: r.choice("gs99"),
            align: r.flag("Algn"),
            scale: r.number("Scl "),
            offset: r.offset("Ofst"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let mut p = Patch {
            d: descriptor,
            order: &GRADIENT_OVERLAY_ORDER,
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("Md  ", self.blend_mode);
        p.percent("Opct", self.opacity);
        p.gradient("Grad", &self.gradient);
        p.angle("Angl", self.angle);
        p.choice("Type", self.style);
        p.flag("Rvrs", self.reverse);
        p.flag("Dthr", self.dither);
        p.choice("gs99", self.interpolation);
        p.flag("Algn", self.align);
        p.percent("Scl ", self.scale);
        p.offset("Ofst", &self.offset);
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("GrFl");
        self.apply_to(&mut descriptor);
        descriptor
    }
}

/// A pattern overlay (`patternFill`).
#[derive(Debug, Clone, PartialEq)]
pub struct PatternOverlay {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    pub blend_mode: Option<BlendMode>,
    /// Percent.
    pub opacity: Option<f64>,
    /// `Ptrn`: absent from files whose pattern Photoshop could not resolve.
    pub pattern: Option<PatternRef>,
    /// `Angl`: degrees, present only in files where it is not zero.
    pub angle: Option<f64>,
    /// `Scl `: percent.
    pub scale: Option<f64>,
    /// `Algn`: link with the layer.
    pub linked: Option<bool>,
    /// `phase`.
    pub phase: Option<Offset>,
}

impl Default for PatternOverlay {
    /// Photoshop's default pattern overlay: normal at 100% and 100% scale, linked, unshifted,
    /// with no pattern chosen.
    fn default() -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            blend_mode: Some(BlendMode::NORMAL),
            opacity: Some(100.0),
            pattern: None,
            angle: None,
            scale: Some(100.0),
            linked: Some(true),
            phase: Some(Offset {
                horizontal: 0.0,
                vertical: 0.0,
                units: crate::OffsetUnits::Percent,
            }),
        }
    }
}

impl PatternOverlay {
    /// Read the fields a pattern-overlay descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            blend_mode: r.blend("Md  "),
            opacity: r.number("Opct"),
            pattern: r.pattern("Ptrn"),
            angle: r.number("Angl"),
            scale: r.number("Scl "),
            linked: r.flag("Algn"),
            phase: r.offset("phase"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let mut p = Patch {
            d: descriptor,
            order: &PATTERN_OVERLAY_ORDER,
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.blend("Md  ", self.blend_mode);
        p.percent("Opct", self.opacity);
        p.pattern("Ptrn", &self.pattern);
        p.angle("Angl", self.angle);
        p.percent("Scl ", self.scale);
        p.flag("Algn", self.linked);
        p.offset("phase", &self.phase);
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("patternFill");
        self.apply_to(&mut descriptor);
        descriptor
    }
}

/// A stroke (`FrFX`, and the entries of `frameFXMulti`).
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub enabled: Option<bool>,
    pub present: Option<bool>,
    pub show_in_dialog: Option<bool>,
    /// `Styl`.
    pub position: Option<StrokePosition>,
    /// `PntT`.
    pub fill: Option<StrokeFill>,
    pub blend_mode: Option<BlendMode>,
    /// Percent.
    pub opacity: Option<f64>,
    /// `Sz  `: pixels.
    pub size: Option<f64>,
    /// `Clr `: the colour of a solid stroke. A gradient stroke keeps a placeholder here.
    pub color: Option<Color>,
    /// `Grad`: a gradient stroke's gradient.
    pub gradient: Option<Gradient>,
    /// `gradientsInterpolationMethod`.
    pub interpolation: Option<GradientInterpolation>,
    /// `Angl`: degrees.
    pub angle: Option<f64>,
    /// `Type`.
    pub style: Option<GradientStyle>,
    /// `Rvrs`.
    pub reverse: Option<bool>,
    /// `Dthr`.
    pub dither: Option<bool>,
    /// `Ptrn`: a pattern stroke's pattern.
    pub pattern: Option<PatternRef>,
    /// `Scl `: percent.
    pub scale: Option<f64>,
    /// `Algn`: align with the layer (gradient stroke).
    pub align: Option<bool>,
    /// `Lnkd`: link with the layer (pattern stroke).
    pub linked: Option<bool>,
    /// `Ofst`: a gradient stroke's offset.
    pub offset: Option<Offset>,
    /// `phase`: a pattern stroke's phase.
    pub phase: Option<Offset>,
    pub overprint: Option<bool>,
}

impl Default for Stroke {
    /// Photoshop's default stroke: 3 px, outside, normal red at 100%.
    fn default() -> Self {
        Self {
            enabled: Some(true),
            present: Some(true),
            show_in_dialog: Some(true),
            position: Some(StrokePosition::Outside),
            fill: Some(StrokeFill::Color),
            blend_mode: Some(BlendMode::NORMAL),
            opacity: Some(100.0),
            size: Some(3.0),
            color: rgb(255.0, 0.0, 0.0),
            gradient: None,
            interpolation: None,
            angle: None,
            style: None,
            reverse: None,
            dither: None,
            pattern: None,
            scale: None,
            align: None,
            linked: None,
            offset: None,
            phase: None,
            overprint: Some(false),
        }
    }
}

impl Stroke {
    /// Read the fields a stroke descriptor holds.
    pub fn from_descriptor(descriptor: &Descriptor) -> Self {
        let r = Read(descriptor);
        Self {
            enabled: r.flag("enab"),
            present: r.flag("present"),
            show_in_dialog: r.flag("showInDialog"),
            position: r.choice("Styl"),
            fill: r.choice("PntT"),
            blend_mode: r.blend("Md  "),
            opacity: r.number("Opct"),
            size: r.number("Sz  "),
            color: r.color("Clr "),
            gradient: r.gradient("Grad"),
            interpolation: r.choice("gradientsInterpolationMethod"),
            angle: r.number("Angl"),
            style: r.choice("Type"),
            reverse: r.flag("Rvrs"),
            dither: r.flag("Dthr"),
            pattern: r.pattern("Ptrn"),
            scale: r.number("Scl "),
            align: r.flag("Algn"),
            linked: r.flag("Lnkd"),
            offset: r.offset("Ofst"),
            phase: r.offset("phase"),
            overprint: r.flag("overprint"),
        }
    }

    /// Write the `Some` fields into `descriptor`, leaving the rest as it is.
    pub fn apply_to(&self, descriptor: &mut Descriptor) {
        let mut p = Patch {
            d: descriptor,
            order: &STROKE_ORDER,
        };
        p.flag("enab", self.enabled);
        p.flag("present", self.present);
        p.flag("showInDialog", self.show_in_dialog);
        p.choice("Styl", self.position);
        p.choice("PntT", self.fill);
        p.blend("Md  ", self.blend_mode);
        p.percent("Opct", self.opacity);
        p.pixels("Sz  ", self.size);
        p.color("Clr ", &self.color);
        p.gradient("Grad", &self.gradient);
        p.choice("gradientsInterpolationMethod", self.interpolation);
        p.angle("Angl", self.angle);
        p.choice("Type", self.style);
        p.flag("Rvrs", self.reverse);
        p.flag("Dthr", self.dither);
        p.pattern("Ptrn", &self.pattern);
        p.percent("Scl ", self.scale);
        p.flag("Algn", self.align);
        p.flag("Lnkd", self.linked);
        p.offset("Ofst", &self.offset);
        p.offset("phase", &self.phase);
        p.flag("overprint", self.overprint);
    }

    /// A fresh descriptor in Photoshop's layout.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut descriptor = Descriptor::with_class("FrFX");
        self.apply_to(&mut descriptor);
        descriptor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(descriptor: &Descriptor) -> Vec<String> {
        descriptor
            .items
            .iter()
            .map(|i| i.key.as_str().trim_end().to_owned())
            .collect()
    }

    #[test]
    fn a_default_drop_shadow_has_photoshops_full_layout() {
        let d = Shadow::new(ShadowKind::Drop).to_descriptor(ShadowKind::Drop);
        assert_eq!(
            keys(&d),
            [
                "enab",
                "present",
                "showInDialog",
                "Md",
                "Clr",
                "Opct",
                "uglg",
                "lagl",
                "Dstn",
                "Ckmt",
                "blur",
                "Nose",
                "AntA",
                "TrnS",
                "layerConceals"
            ]
        );
        // Four-byte keys are character IDs; the longer ones are explicit.
        let encoding: Vec<bool> = d
            .items
            .iter()
            .map(|i| i.key.uses_implicit_length())
            .collect();
        assert!(encoding[0]); // enab
        assert!(!encoding[1]); // present
        assert!(!encoding[14]); // layerConceals
    }

    #[test]
    fn an_inner_shadow_has_no_knockout_item() {
        let d = Shadow::new(ShadowKind::Inner).to_descriptor(ShadowKind::Inner);
        assert_eq!(keys(&d).len(), 14);
        assert!(!d.contains("layerConceals"));
    }

    #[test]
    fn every_default_effect_survives_a_write_and_a_read() {
        let shadow = Shadow::new(ShadowKind::Drop);
        assert_eq!(
            Shadow::from_descriptor(&shadow.to_descriptor(ShadowKind::Drop)),
            shadow
        );
        let glow = Glow::new(GlowKind::Inner);
        assert_eq!(
            Glow::from_descriptor(&glow.to_descriptor(GlowKind::Inner)),
            glow
        );
        let bevel = Bevel::default();
        assert_eq!(Bevel::from_descriptor(&bevel.to_descriptor()), bevel);
        let overlay = ColorOverlay::default();
        assert_eq!(
            ColorOverlay::from_descriptor(&overlay.to_descriptor()),
            overlay
        );
        let satin = Satin::default();
        assert_eq!(Satin::from_descriptor(&satin.to_descriptor()), satin);
        let gradient = GradientOverlay::default();
        assert_eq!(
            GradientOverlay::from_descriptor(&gradient.to_descriptor()),
            gradient
        );
        let pattern = PatternOverlay::default();
        assert_eq!(
            PatternOverlay::from_descriptor(&pattern.to_descriptor()),
            pattern
        );
        let stroke = Stroke::default();
        assert_eq!(Stroke::from_descriptor(&stroke.to_descriptor()), stroke);
    }

    #[test]
    fn a_gradient_overlay_has_the_full_layout_photoshop_requires() {
        let d = GradientOverlay::default().to_descriptor();
        assert_eq!(
            keys(&d),
            [
                "enab",
                "present",
                "showInDialog",
                "Md",
                "Opct",
                "Grad",
                "Angl",
                "Type",
                "Rvrs",
                "Dthr",
                "gs99",
                "Algn",
                "Scl",
                "Ofst"
            ]
        );
    }

    #[test]
    fn setting_a_field_inserts_it_where_photoshop_puts_it_and_leaves_the_rest() {
        // A shadow from an older file: no `present`, no `showInDialog`, plus an unknown item.
        let mut d = Shadow::new(ShadowKind::Inner).to_descriptor(ShadowKind::Inner);
        d.remove("present");
        d.remove("showInDialog");
        d.set("futureField", DescriptorValue::long(7));
        let before = keys(&d);

        let edit = Shadow {
            present: Some(true),
            ..Shadow::from_descriptor(&d)
        };
        edit.apply_to(&mut d, ShadowKind::Inner);
        let after = keys(&d);
        assert_eq!(
            after[..3],
            ["enab", "present", "Md"],
            "`present` lands after `enab`"
        );
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(
            after.last().unwrap(),
            "futureField",
            "the unknown item stays where it was"
        );
    }

    #[test]
    fn a_no_op_edit_changes_nothing_even_when_the_file_spells_values_differently() {
        let mut d = Shadow::new(ShadowKind::Drop).to_descriptor(ShadowKind::Drop);
        // The 2026 long-form blend mode and a percent stored under another unit.
        d.set("Md  ", DescriptorValue::enumerated("BlnM", "multiply"));
        d.set("Nose", DescriptorValue::pixels(0.0));
        let original = d.clone();
        Shadow::from_descriptor(&d).apply_to(&mut d, ShadowKind::Drop);
        assert_eq!(d, original);
    }

    #[test]
    fn changing_a_field_rewrites_only_that_field() {
        let mut d = Stroke::default().to_descriptor();
        let mut edit = Stroke::from_descriptor(&d);
        edit.size = Some(9.0);
        edit.color = rgb(0.0, 128.0, 255.0);
        edit.apply_to(&mut d);
        let read = Stroke::from_descriptor(&d);
        assert_eq!(read.size, Some(9.0));
        assert_eq!(read.color, rgb(0.0, 128.0, 255.0));
        assert_eq!(read.opacity, Some(100.0));
    }

    #[test]
    fn unknown_enumerators_and_shapes_read_as_none_and_are_left_alone() {
        let mut d = Bevel::default().to_descriptor();
        d.set("bvlS", DescriptorValue::enumerated("BESl", "futureStyle"));
        let read = Bevel::from_descriptor(&d);
        assert_eq!(read.style, None);
        let before = d.clone();
        read.apply_to(&mut d);
        assert_eq!(d, before);
    }
}
