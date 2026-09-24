use std::path::Path;

use psd::core::{BlendMode, EffectKind, LayerEffectsData, TaggedBlockKey};
use psd::LayeredFile;

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/documents/SmartObjects/smart_object_file_no_warp.psd")
}

#[test]
fn reads_modern_and_legacy_effects_without_changing_blocks() {
    let file = LayeredFile::<u8>::read(fixture()).unwrap();
    let layers: Vec<_> = file
        .layers()
        .filter(|layer| !layer.effects().unwrap().is_empty())
        .collect();
    assert_eq!(layers.len(), 2);
    for layer in &layers {
        let effects = layer.effects().unwrap();
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].key, TaggedBlockKey::new(*b"lfx2"));
        assert_eq!(effects[1].key, TaggedBlockKey::new(*b"lrFX"));
        let LayerEffectsData::Modern(modern) = &effects[0].data else {
            panic!("expected modern effects");
        };
        let entries = modern.effects().unwrap();
        for kind in [
            EffectKind::DropShadow,
            EffectKind::InnerShadow,
            EffectKind::OuterGlow,
            EffectKind::InnerGlow,
            EffectKind::Bevel,
            EffectKind::SolidFill,
            EffectKind::Satin,
            EffectKind::GradientOverlay,
            EffectKind::Stroke,
        ] {
            assert!(
                entries.iter().any(|effect| effect.kind == kind),
                "missing {kind:?}"
            );
        }
        assert!(entries.iter().any(|effect| effect.enabled().is_some()));
        // Every blend mode in this Photoshop 2022 file uses a historical ID
        // (`Mltp`, `linearBurn`), and each one decodes.
        for effect in &entries {
            if let Some((_, raw)) = effect.blend_mode() {
                assert!(effect.blend_mode_value().is_some(), "{raw:?}");
            }
        }
        let shadow = entries
            .iter()
            .find(|effect| effect.kind == EffectKind::DropShadow)
            .unwrap();
        let expected = match shadow.blend_mode().unwrap().1.as_bytes() {
            b"Nrml" => BlendMode::NORMAL,
            b"Mltp" => BlendMode::MULTIPLY,
            other => panic!("unexpected shadow mode {other:?}"),
        };
        assert_eq!(shadow.blend_mode_value(), Some(expected));
        assert!(entries
            .iter()
            .any(|effect| effect.opacity_percent().is_some()));
        assert!(modern.enabled().is_some());
        let LayerEffectsData::Legacy(legacy) = &effects[1].data else {
            panic!("expected legacy effects");
        };
        assert_eq!(legacy.records.len(), 7);
        assert_eq!(legacy.records[0].kind, EffectKind::CommonState);
        assert!(legacy
            .records
            .iter()
            .any(|effect| effect.kind == EffectKind::DropShadow));
        assert!(legacy.records.iter().any(|effect| effect.enabled.is_some()));
        let shadow = legacy
            .records
            .iter()
            .find(|effect| effect.kind == EffectKind::DropShadow)
            .unwrap();
        assert!(shadow.size.is_some());
        assert!(shadow.distance.is_some());
        assert!(shadow.opacity.is_some());
        assert!(shadow.color.is_some());
    }
    assert!(layers.iter().any(|layer| {
        let blocks = layer.effects().unwrap();
        let LayerEffectsData::Modern(modern) = &blocks[0].data else {
            return false;
        };
        modern
            .effects()
            .unwrap()
            .iter()
            .any(|effect| effect.kind == EffectKind::PatternOverlay)
    }));

    let before: Vec<_> = file
        .layers()
        .map(|layer| {
            (
                layer.name.clone(),
                layer
                    .blocks
                    .blocks
                    .iter()
                    .filter(|block| matches!(&block.key.as_bytes(), b"lfx2" | b"lrFX"))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let back = LayeredFile::<u8>::from_bytes(&file.to_bytes().unwrap()).unwrap();
    let after: Vec<_> = back
        .layers()
        .map(|layer| {
            (
                layer.name.clone(),
                layer
                    .blocks
                    .blocks
                    .iter()
                    .filter(|block| matches!(&block.key.as_bytes(), b"lfx2" | b"lrFX"))
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    assert_eq!(after, before);
}
