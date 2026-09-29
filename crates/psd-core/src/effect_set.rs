//! The whole set of effects on a layer: the root of an `lfx2`, `lmfx` or `lfxs` descriptor.
//!
//! [`LayerEffects`] holds every effect the layer has, as the models in [`effects`](crate::effects),
//! and reads from, patches and builds the root descriptor around them. Like the models it holds,
//! it edits a descriptor in place: unknown root items stay, and so do the effects' own unknown
//! items and any effect the edit does not touch.
//!
//! One thing differs from a single effect. A field left `None` in an effect means "leave it",
//! but here the set is the whole truth about what the layer has: an effect family with no
//! entries (an empty list, a `None` glow) is removed from the descriptor, and an entry with no
//! counterpart in the file is added.
//!
//! Effects that can repeat (shadows, colour and gradient overlays, strokes) come in two forms
//! in Photoshop files: a single key (`DrSh`) or a list (`dropShadowMulti`). Files from
//! Photoshop CC 2015 on use the list even for one effect; older ones use the single key. An
//! edit keeps whichever form the file used. A new set uses the single key for one effect and
//! the list for several, which every Photoshop version reads.

use crate::descriptor::{Descriptor, DescriptorValue};
use crate::descriptor_build::UNIT_PERCENT;
use crate::effects::{
    Bevel, ColorOverlay, Glow, GlowKind, GradientOverlay, PatternOverlay, Satin, Shadow,
    ShadowKind, Stroke,
};
use crate::error::Result;
use crate::io::BeWriter;

/// The root items in the order Photoshop writes them.
const ROOT_ORDER: [&str; 18] = [
    "Scl ",
    "masterFXSwitch",
    "DrSh",
    "dropShadowMulti",
    "IrSh",
    "innerShadowMulti",
    "OrGl",
    "SoFi",
    "solidFillMulti",
    "GrFl",
    "gradientFillMulti",
    "patternFill",
    "FrFX",
    "frameFXMulti",
    "IrGl",
    "ebbl",
    "ChFX",
    "numModifyingFX",
];

/// Every effect on a layer.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerEffects {
    /// `Scl `: the scale the effects were authored at, percent. Photoshop scales effect sizes
    /// by it when the document is resampled.
    pub scale: Option<f64>,
    /// `masterFXSwitch`: `false` when the layer's effects are switched off as a whole.
    pub master_switch: Option<bool>,
    pub drop_shadows: Vec<Shadow>,
    pub inner_shadows: Vec<Shadow>,
    pub outer_glow: Option<Glow>,
    pub inner_glow: Option<Glow>,
    pub bevel: Option<Bevel>,
    pub color_overlays: Vec<ColorOverlay>,
    pub satin: Option<Satin>,
    pub gradient_overlays: Vec<GradientOverlay>,
    pub pattern_overlay: Option<PatternOverlay>,
    pub strokes: Vec<Stroke>,
}

impl Default for LayerEffects {
    /// No effects, switched on, at 100% scale.
    fn default() -> Self {
        Self {
            scale: Some(100.0),
            master_switch: Some(true),
            drop_shadows: Vec::new(),
            inner_shadows: Vec::new(),
            outer_glow: None,
            inner_glow: None,
            bevel: None,
            color_overlays: Vec::new(),
            satin: None,
            gradient_overlays: Vec::new(),
            pattern_overlay: None,
            strokes: Vec::new(),
        }
    }
}

/// The entries of a repeatable effect: the list if the descriptor has one, else the single key.
fn read_family<M>(
    root: &Descriptor,
    single: &str,
    multi: &str,
    read: impl Fn(&Descriptor) -> M,
) -> Vec<M> {
    if let Some(list) = root.get(multi).and_then(DescriptorValue::as_list) {
        list.iter()
            .filter_map(DescriptorValue::as_descriptor)
            .map(read)
            .collect()
    } else if let Some(one) = root.get(single).and_then(DescriptorValue::as_descriptor) {
        vec![read(one)]
    } else {
        Vec::new()
    }
}

/// Make the descriptor hold exactly `items` for a repeatable effect, keeping the form (single
/// key or list) it already uses and patching existing entries in place by position.
fn write_family<M>(
    root: &mut Descriptor,
    single: &str,
    multi: &str,
    items: &[M],
    apply: impl Fn(&M, &mut Descriptor),
    fresh: impl Fn(&M) -> Descriptor,
) {
    if items.is_empty() {
        root.remove(single);
        root.remove(multi);
        return;
    }
    let list_form = root.get(multi).is_some();
    if !list_form && items.len() == 1 {
        // Single key: patch the existing entry where it stands, or add one.
        if let Some(DescriptorValue::Descriptor(entry)) = root.get_mut(single) {
            apply(&items[0], entry);
        } else {
            let entry = DescriptorValue::Descriptor(fresh(&items[0]));
            root.set_ordered(single, entry, &ROOT_ORDER);
        }
        return;
    }
    // List form. The list is authoritative when there is one, so a single key beside it goes;
    // with no list, the single entry becomes the list's first.
    let existing_single = match root.remove(single) {
        Some(DescriptorValue::Descriptor(d)) => Some(d),
        _ => None,
    };
    let mut existing: Vec<Descriptor> = match root.get(multi).and_then(DescriptorValue::as_list) {
        Some(list) => list
            .iter()
            .filter_map(|v| v.as_descriptor().cloned())
            .collect(),
        None => existing_single.into_iter().collect(),
    };
    existing.truncate(items.len());
    let entries = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            DescriptorValue::Descriptor(match existing.get(index) {
                Some(current) => {
                    let mut d = current.clone();
                    apply(item, &mut d);
                    d
                }
                None => fresh(item),
            })
        })
        .collect();
    root.set_ordered(multi, DescriptorValue::List(entries), &ROOT_ORDER);
}

/// Make the descriptor hold `effect` under `key`, or nothing when it is `None`.
fn write_single<M>(
    root: &mut Descriptor,
    key: &str,
    effect: &Option<M>,
    apply: impl Fn(&M, &mut Descriptor),
    fresh: impl Fn(&M) -> Descriptor,
) {
    match effect {
        None => {
            root.remove(key);
        }
        Some(effect) => {
            if let Some(DescriptorValue::Descriptor(existing)) = root.get_mut(key) {
                apply(effect, existing);
            } else {
                root.set_ordered(key, DescriptorValue::Descriptor(fresh(effect)), &ROOT_ORDER);
            }
        }
    }
}

fn read_single<M>(root: &Descriptor, key: &str, read: impl Fn(&Descriptor) -> M) -> Option<M> {
    root.get(key)
        .and_then(DescriptorValue::as_descriptor)
        .map(read)
}

impl LayerEffects {
    /// Read the effects a root descriptor holds.
    pub fn from_descriptor(root: &Descriptor) -> Self {
        Self {
            scale: crate::color::number(root, "Scl "),
            master_switch: root.get_bool("masterFXSwitch"),
            drop_shadows: read_family(root, "DrSh", "dropShadowMulti", Shadow::from_descriptor),
            inner_shadows: read_family(root, "IrSh", "innerShadowMulti", Shadow::from_descriptor),
            outer_glow: read_single(root, "OrGl", Glow::from_descriptor),
            inner_glow: read_single(root, "IrGl", Glow::from_descriptor),
            bevel: read_single(root, "ebbl", Bevel::from_descriptor),
            color_overlays: read_family(
                root,
                "SoFi",
                "solidFillMulti",
                ColorOverlay::from_descriptor,
            ),
            satin: read_single(root, "ChFX", Satin::from_descriptor),
            gradient_overlays: read_family(
                root,
                "GrFl",
                "gradientFillMulti",
                GradientOverlay::from_descriptor,
            ),
            pattern_overlay: read_single(root, "patternFill", PatternOverlay::from_descriptor),
            strokes: read_family(root, "FrFX", "frameFXMulti", Stroke::from_descriptor),
        }
    }

    /// Make `root` describe exactly these effects, editing it in place.
    ///
    /// `numModifyingFX`, the tally of enabled effects that newer files carry, moves by however
    /// much the edit changed the number of enabled effects when the descriptor has it, and
    /// stays absent when it does not. It is never recomputed, because Photoshop's own value is
    /// not always the plain count.
    pub fn apply_to(&self, root: &mut Descriptor) {
        self.apply(root, false);
    }

    /// A fresh root descriptor in Photoshop's layout, including `numModifyingFX`.
    pub fn to_descriptor(&self) -> Descriptor {
        let mut root = Descriptor::with_class("null");
        self.apply(&mut root, true);
        root
    }

    fn apply(&self, root: &mut Descriptor, count_modifying: bool) {
        let enabled_before = Self::from_descriptor(root).modifying_count();
        if let Some(scale) = self.scale {
            if crate::color::number(root, "Scl ") != Some(scale) {
                root.set_ordered(
                    "Scl ",
                    DescriptorValue::unit(UNIT_PERCENT, scale),
                    &ROOT_ORDER,
                );
            }
        }
        if let Some(on) = self.master_switch {
            if root.get_bool("masterFXSwitch") != Some(on) {
                root.set_ordered("masterFXSwitch", DescriptorValue::boolean(on), &ROOT_ORDER);
            }
        }
        write_family(
            root,
            "DrSh",
            "dropShadowMulti",
            &self.drop_shadows,
            |m, d| m.apply_to(d, ShadowKind::Drop),
            |m| m.to_descriptor(ShadowKind::Drop),
        );
        write_family(
            root,
            "IrSh",
            "innerShadowMulti",
            &self.inner_shadows,
            |m, d| m.apply_to(d, ShadowKind::Inner),
            |m| m.to_descriptor(ShadowKind::Inner),
        );
        write_single(
            root,
            "OrGl",
            &self.outer_glow,
            |m, d| m.apply_to(d, GlowKind::Outer),
            |m| m.to_descriptor(GlowKind::Outer),
        );
        write_family(
            root,
            "SoFi",
            "solidFillMulti",
            &self.color_overlays,
            ColorOverlay::apply_to,
            ColorOverlay::to_descriptor,
        );
        write_family(
            root,
            "GrFl",
            "gradientFillMulti",
            &self.gradient_overlays,
            GradientOverlay::apply_to,
            GradientOverlay::to_descriptor,
        );
        write_single(
            root,
            "patternFill",
            &self.pattern_overlay,
            PatternOverlay::apply_to,
            PatternOverlay::to_descriptor,
        );
        write_family(
            root,
            "FrFX",
            "frameFXMulti",
            &self.strokes,
            Stroke::apply_to,
            Stroke::to_descriptor,
        );
        write_single(
            root,
            "IrGl",
            &self.inner_glow,
            |m, d| m.apply_to(d, GlowKind::Inner),
            |m| m.to_descriptor(GlowKind::Inner),
        );
        write_single(
            root,
            "ebbl",
            &self.bevel,
            Bevel::apply_to,
            Bevel::to_descriptor,
        );
        write_single(
            root,
            "ChFX",
            &self.satin,
            Satin::apply_to,
            Satin::to_descriptor,
        );

        // `numModifyingFX` is Photoshop's own tally and is not always the number of enabled
        // effects (a stroke of no width is enabled but modifies nothing, and the file says 0),
        // so an existing value is moved by however much this edit changed the enabled count and
        // is never recomputed: an edit that changes nothing leaves it exactly as it was.
        let after = self.modifying_count();
        match root.get_long("numModifyingFX") {
            Some(stored) if enabled_before != after => {
                let moved = i64::from(stored) + after as i64 - enabled_before as i64;
                let moved = i32::try_from(moved.max(0)).unwrap_or(i32::MAX);
                root.set_ordered("numModifyingFX", DescriptorValue::long(moved), &ROOT_ORDER);
            }
            None if count_modifying => {
                let count = i32::try_from(after).unwrap_or(i32::MAX);
                root.set_ordered("numModifyingFX", DescriptorValue::long(count), &ROOT_ORDER);
            }
            _ => {}
        }
    }

    /// How many effects are enabled, which is what Photoshop's `numModifyingFX` usually holds.
    pub fn modifying_count(&self) -> usize {
        fn on(enabled: Option<bool>) -> usize {
            usize::from(enabled == Some(true))
        }
        let glow = |g: &Option<Glow>| g.as_ref().map_or(0, |g| on(g.enabled));
        self.drop_shadows
            .iter()
            .map(|e| on(e.enabled))
            .sum::<usize>()
            + self
                .inner_shadows
                .iter()
                .map(|e| on(e.enabled))
                .sum::<usize>()
            + glow(&self.outer_glow)
            + glow(&self.inner_glow)
            + self.bevel.as_ref().map_or(0, |e| on(e.enabled))
            + self
                .color_overlays
                .iter()
                .map(|e| on(e.enabled))
                .sum::<usize>()
            + self.satin.as_ref().map_or(0, |e| on(e.enabled))
            + self
                .gradient_overlays
                .iter()
                .map(|e| on(e.enabled))
                .sum::<usize>()
            + self.pattern_overlay.as_ref().map_or(0, |e| on(e.enabled))
            + self.strokes.iter().map(|e| on(e.enabled)).sum::<usize>()
    }

    /// Whether any repeatable effect has more than one instance. Photoshop puts such a set in
    /// an `lmfx` block.
    pub fn has_multiple_instances(&self) -> bool {
        self.drop_shadows.len() > 1
            || self.inner_shadows.len() > 1
            || self.color_overlays.len() > 1
            || self.gradient_overlays.len() > 1
            || self.strokes.len() > 1
    }

    /// Whether the set holds no effect at all.
    pub fn is_empty(&self) -> bool {
        self.drop_shadows.is_empty()
            && self.inner_shadows.is_empty()
            && self.outer_glow.is_none()
            && self.inner_glow.is_none()
            && self.bevel.is_none()
            && self.color_overlays.is_empty()
            && self.satin.is_none()
            && self.gradient_overlays.is_empty()
            && self.pattern_overlay.is_none()
            && self.strokes.is_empty()
    }
}

/// The payload of an effects block: version 0, descriptor version 16, the descriptor, and zero
/// padding to a multiple of four, which Photoshop counts in the block's length (every
/// effects block in Photoshop-authored files has such a length).
pub fn effects_block_data(root: &Descriptor) -> Result<Vec<u8>> {
    let mut writer = BeWriter::new();
    writer.u32(0);
    writer.u32(16);
    root.write(&mut writer)?;
    let mut data = writer.into_inner();
    data.resize(data.len().next_multiple_of(4), 0);
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::BeReader;

    fn keys(d: &Descriptor) -> Vec<String> {
        d.items
            .iter()
            .map(|i| i.key.as_str().trim_end().to_owned())
            .collect()
    }

    fn one_of_each() -> LayerEffects {
        LayerEffects {
            drop_shadows: vec![Shadow::new(ShadowKind::Drop)],
            inner_shadows: vec![Shadow::new(ShadowKind::Inner)],
            outer_glow: Some(Glow::new(GlowKind::Outer)),
            inner_glow: Some(Glow::new(GlowKind::Inner)),
            bevel: Some(Bevel::default()),
            color_overlays: vec![ColorOverlay::default()],
            satin: Some(Satin::default()),
            gradient_overlays: vec![GradientOverlay::default()],
            pattern_overlay: Some(PatternOverlay::default()),
            strokes: vec![Stroke::default()],
            ..LayerEffects::default()
        }
    }

    #[test]
    fn a_full_set_round_trips_and_is_laid_out_the_way_photoshop_lays_it_out() {
        let effects = one_of_each();
        let root = effects.to_descriptor();
        assert_eq!(LayerEffects::from_descriptor(&root), effects);
        assert_eq!(
            keys(&root),
            [
                "Scl",
                "masterFXSwitch",
                "DrSh",
                "IrSh",
                "OrGl",
                "SoFi",
                "GrFl",
                "patternFill",
                "FrFX",
                "IrGl",
                "ebbl",
                "ChFX",
                "numModifyingFX"
            ]
        );
        assert_eq!(root.get_long("numModifyingFX"), Some(10));
        assert_eq!(root.class_id.as_bytes(), b"null");
    }

    #[test]
    fn several_instances_use_the_list_and_one_uses_the_single_key() {
        let effects = LayerEffects {
            strokes: vec![Stroke::default(), Stroke::default()],
            drop_shadows: vec![Shadow::new(ShadowKind::Drop)],
            ..LayerEffects::default()
        };
        let root = effects.to_descriptor();
        assert!(root.contains("frameFXMulti") && !root.contains("FrFX"));
        assert!(root.contains("DrSh") && !root.contains("dropShadowMulti"));
        assert!(effects.has_multiple_instances());
        assert_eq!(LayerEffects::from_descriptor(&root), effects);
    }

    #[test]
    fn a_list_of_one_stays_a_list_when_the_set_is_edited() {
        // Newer files write one shadow as a list of one.
        let mut root = Descriptor::with_class("null");
        root.set(
            "dropShadowMulti",
            DescriptorValue::List(vec![DescriptorValue::Descriptor(
                Shadow::new(ShadowKind::Drop).to_descriptor(ShadowKind::Drop),
            )]),
        );
        let mut effects = LayerEffects::from_descriptor(&root);
        effects.drop_shadows[0].size = Some(20.0);
        effects.apply_to(&mut root);
        assert!(root.contains("dropShadowMulti") && !root.contains("DrSh"));
        assert_eq!(
            LayerEffects::from_descriptor(&root).drop_shadows[0].size,
            Some(20.0)
        );
    }

    #[test]
    fn growing_a_single_effect_converts_it_to_a_list_keeping_its_unknown_items() {
        let mut root = Descriptor::with_class("null");
        let mut shadow = Shadow::new(ShadowKind::Drop).to_descriptor(ShadowKind::Drop);
        shadow.set("kept", DescriptorValue::long(3));
        root.set("DrSh", DescriptorValue::Descriptor(shadow));

        let mut effects = LayerEffects::from_descriptor(&root);
        effects.drop_shadows.push(Shadow::new(ShadowKind::Drop));
        effects.apply_to(&mut root);

        assert!(!root.contains("DrSh"));
        let list = root.get("dropShadowMulti").unwrap().as_list().unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].as_descriptor().unwrap().contains("kept"));
    }

    #[test]
    fn a_family_with_no_entries_is_removed_and_the_rest_left_alone() {
        let mut root = one_of_each().to_descriptor();
        root.set("unknownRootItem", DescriptorValue::long(1));
        let mut effects = LayerEffects::from_descriptor(&root);
        effects.drop_shadows.clear();
        effects.bevel = None;
        effects.apply_to(&mut root);
        assert!(!root.contains("DrSh") && !root.contains("ebbl"));
        assert!(root.contains("IrSh") && root.contains("unknownRootItem"));
        assert_eq!(root.get_long("numModifyingFX"), Some(8));
    }

    #[test]
    fn a_no_op_edit_changes_no_bytes() {
        let mut root = one_of_each().to_descriptor();
        root.set("unknownRootItem", DescriptorValue::long(1));
        let before = root.clone();
        LayerEffects::from_descriptor(&root).apply_to(&mut root);
        assert_eq!(root, before);
    }

    #[test]
    fn num_modifying_fx_is_only_added_where_it_is_wanted() {
        let mut root = Descriptor::with_class("null");
        let effects = LayerEffects {
            strokes: vec![Stroke::default()],
            ..LayerEffects::default()
        };
        effects.apply_to(&mut root);
        assert!(
            !root.contains("numModifyingFX"),
            "an older-style root stays without it"
        );
        assert!(effects.to_descriptor().contains("numModifyingFX"));
    }

    #[test]
    fn the_stored_tally_moves_by_the_change_and_is_not_recomputed() {
        // Photoshop's tally is not always the enabled count (a zero-width stroke is enabled
        // but counted as 0), so a stored 5 over 3 enabled effects is left alone by a no-op
        // and follows a real change by its size.
        let mut effects = LayerEffects {
            drop_shadows: vec![Shadow::new(ShadowKind::Drop)],
            satin: Some(Satin::default()),
            strokes: vec![Stroke::default()],
            ..LayerEffects::default()
        };
        let mut root = effects.to_descriptor();
        root.set("numModifyingFX", DescriptorValue::long(5));

        effects.apply_to(&mut root);
        assert_eq!(root.get_long("numModifyingFX"), Some(5));

        effects.satin.as_mut().unwrap().enabled = Some(false);
        effects.apply_to(&mut root);
        assert_eq!(root.get_long("numModifyingFX"), Some(4));

        effects.satin = None;
        effects.strokes.clear();
        effects.drop_shadows.clear();
        effects.apply_to(&mut root);
        assert_eq!(
            root.get_long("numModifyingFX"),
            Some(2),
            "every enabled effect removed: 4 - 2"
        );
        effects.drop_shadows = vec![Shadow::new(ShadowKind::Drop); 9];
        effects.apply_to(&mut root);
        assert_eq!(root.get_long("numModifyingFX"), Some(11));
    }

    #[test]
    fn a_no_op_edit_keeps_a_root_in_a_non_canonical_order() {
        // Older files order their items differently; an edit that changes nothing must not
        // tidy them.
        let mut root = one_of_each().to_descriptor();
        root.items.reverse();
        let before = root.clone();
        LayerEffects::from_descriptor(&root).apply_to(&mut root);
        assert_eq!(root, before);
    }

    #[test]
    fn the_block_payload_is_versioned_and_padded_to_a_multiple_of_four() {
        let root = one_of_each().to_descriptor();
        let data = effects_block_data(&root).unwrap();
        assert_eq!(data.len() % 4, 0);
        let mut reader = BeReader::new(&data);
        assert_eq!((reader.u32().unwrap(), reader.u32().unwrap()), (0, 16));
        assert_eq!(Descriptor::read(&mut reader).unwrap(), root);
    }
}
