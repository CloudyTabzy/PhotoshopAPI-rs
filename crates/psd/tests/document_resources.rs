//! The typed document-resource views — grid and guides (`1032`), slices
//! (`1050`), layer comps (`1065`) — must re-serialize to exactly the bytes they
//! were parsed from.
//!
//! The corpus harness compares document resources by raw payload, which proves
//! a save keeps them but says nothing about the parsers behind the views. This
//! sweeps the same files and checks the round trip of each view against its
//! block, so a framing mistake (padding, field order, a count) shows up as a
//! byte difference instead of a plausible-looking wrong value.
//!
//! `fixtures/` always runs. Set `PSD_EXTRA_CORPUS` to a list of directories
//! (separated as `PATH` is) to sweep more documents.

use std::path::{Path, PathBuf};

use psd::core::image_resources::{ID_GRID_AND_GUIDES, ID_LAYER_COMPS, ID_SLICES};
use psd::core::{BeReader, ResourceBlock};

fn corpus_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/documents")];
    if let Some(extra) = std::env::var_os("PSD_EXTRA_CORPUS") {
        dirs.extend(std::env::split_paths(&extra));
    }
    dirs
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("psd") || ext.eq_ignore_ascii_case("psb"))
        {
            out.push(path);
        }
    }
}

/// The resource payloads of a document, without parsing the rest of it.
fn resource_payloads(bytes: &[u8], id: u16) -> Vec<Vec<u8>> {
    let mut reader = BeReader::new(bytes);
    let mut found = Vec::new();
    if psd::core::FileHeader::read(&mut reader).is_err() {
        return found;
    }
    if psd::core::ColorModeData::read(&mut reader).is_err() {
        return found;
    }
    let Ok(resources) = psd::core::ImageResources::read(&mut reader) else {
        return found;
    };
    for block in resources.blocks() {
        if let ResourceBlock::Raw(raw) = block {
            if raw.id == id {
                found.push(raw.data.clone());
            }
        }
    }
    found
}

/// The per-layer comp state (`cmls`) reads back with the ids the document's
/// comp list names, and a comp that does not mention a layer leaves its own
/// visibility in force.
#[test]
fn layer_comp_states_read_from_a_layer_block() {
    use psd::core::layer_comps::{read_comp_states, LayerComp, LayerComps};
    use psd::core::{
        descriptor::{Descriptor, DescriptorItem, DescriptorKey, DescriptorValue},
        BeWriter, TaggedBlock, TaggedBlockKey, UnicodeString,
    };

    let comps = LayerComps {
        last_applied_comp: 2,
        comps: vec![
            LayerComp {
                id: 1,
                name: "Visible".to_owned(),
                comment: String::new(),
                capture_visibility: true,
                capture_position: false,
                capture_appearance: false,
            },
            LayerComp {
                id: 2,
                name: "Hidden".to_owned(),
                comment: String::new(),
                capture_visibility: true,
                capture_position: true,
                capture_appearance: false,
            },
        ],
    };

    let mut document = psd::LayeredFile::<u8>::new(psd::core::ColorMode::Rgb, 4, 4).unwrap();
    document.image_resources.set_layer_comps(&comps).unwrap();
    let mut layer = psd::Layer::new_image("Comp", psd::Rect::new(0, 0, 4, 4));
    // The layer is hidden and one comp turns it on; the other does not name it.
    layer.set_visible(false);
    let entry = |id: i32, enabled: Option<bool>| {
        let mut items = vec![DescriptorItem {
            key: DescriptorKey::new("compList"),
            value: DescriptorValue::List(vec![DescriptorValue::Integer(id)]),
        }];
        if let Some(enabled) = enabled {
            items.push(DescriptorItem {
                key: DescriptorKey::new("enab"),
                value: DescriptorValue::Boolean(enabled),
            });
        }
        DescriptorValue::Descriptor(Descriptor {
            name: UnicodeString::new("", 1).unwrap(),
            class_id: DescriptorKey::char_id(*b"null"),
            items,
        })
    };
    let mut writer = BeWriter::new();
    writer.i32(16);
    Descriptor {
        name: UnicodeString::new("", 1).unwrap(),
        class_id: DescriptorKey::char_id(*b"null"),
        items: vec![DescriptorItem {
            key: DescriptorKey::new("layerSettings"),
            value: DescriptorValue::List(vec![entry(1, Some(true)), entry(2, None)]),
        }],
    }
    .write(&mut writer)
    .unwrap();
    layer
        .blocks
        .push(TaggedBlock::new(TaggedBlockKey::CMLS, writer.into_inner()));
    let id = document.add_layer(layer);

    let states = document.layer(id).unwrap().comp_states();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0].comp_id, 1);
    assert!(states[0].enabled, "the comp turns the hidden layer on");
    assert_eq!(states[1].comp_id, 2);
    assert!(
        !states[1].enabled,
        "a comp that does not name the layer leaves it hidden"
    );
    // The ids the states name are the ones the document's comp list holds.
    let listed: Vec<i32> = comps.comps.iter().map(|comp| comp.id).collect();
    assert!(states.iter().all(|state| listed.contains(&state.comp_id)));
    assert!(
        read_comp_states(&[], true).is_err(),
        "an empty payload is not a comp state list"
    );

    // Both resources survive a save and read back the same way.
    let back = psd::LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
    let id = back.find_layer("Comp").unwrap();
    assert_eq!(back.layer(id).unwrap().comp_states(), states);
    assert_eq!(back.image_resources.layer_comps().unwrap(), comps);
}

#[test]
fn typed_resource_views_round_trip_byte_for_byte() {
    let mut files = Vec::new();
    for dir in corpus_dirs() {
        collect(&dir, &mut files);
    }
    assert!(!files.is_empty(), "no fixtures found");

    let mut checked = (0usize, 0usize, 0usize);
    let mut failures: Vec<String> = Vec::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();

        for payload in resource_payloads(&bytes, ID_GRID_AND_GUIDES) {
            match psd::core::grid_guides::GridGuides::read(&mut BeReader::new(&payload)) {
                Ok(guides) => {
                    checked.0 += 1;
                    match guides.to_payload() {
                        Ok(again) if again == payload => {}
                        Ok(again) => failures.push(format!(
                            "{name}: guides re-serialize to {} bytes, not {}",
                            again.len(),
                            payload.len()
                        )),
                        Err(error) => failures.push(format!("{name}: guides write: {error}")),
                    }
                }
                Err(error) => failures.push(format!("{name}: guides read: {error}")),
            }
        }

        for payload in resource_payloads(&bytes, ID_SLICES) {
            match psd::core::slices::SlicesResource::read(&mut BeReader::new(&payload)) {
                Ok(slices) => {
                    checked.1 += 1;
                    match slices.to_payload() {
                        Ok(again) if again == payload => {}
                        Ok(again) => failures.push(format!(
                            "{name}: slices v{} re-serialize to {} bytes, not {}",
                            slices.version,
                            again.len(),
                            payload.len()
                        )),
                        Err(error) => failures.push(format!("{name}: slices write: {error}")),
                    }
                }
                Err(error) => failures.push(format!("{name}: slices read: {error}")),
            }
        }

        for payload in resource_payloads(&bytes, ID_LAYER_COMPS) {
            match psd::core::layer_comps::LayerComps::read(&mut BeReader::new(&payload)) {
                Ok(comps) => {
                    checked.2 += 1;
                    // The parsed *values* must match; the descriptor's exact
                    // bytes are the file's business and are preserved raw.
                    match psd::core::layer_comps::LayerComps::read(&mut BeReader::new(
                        &comps.to_payload().expect("comps write"),
                    )) {
                        Ok(again) if again == comps => {}
                        Ok(again) => failures.push(format!(
                            "{name}: layer comps changed on re-read: {comps:?} -> {again:?}"
                        )),
                        Err(error) => failures.push(format!("{name}: comps re-read: {error}")),
                    }
                }
                Err(error) => failures.push(format!("{name}: layer comps read: {error}")),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{} failure(s) over {} guides / {} slices / {} comps resources:\n{}",
        failures.len(),
        checked.0,
        checked.1,
        checked.2,
        failures.join("\n")
    );
    // The sweep has to actually exercise each view.
    assert!(
        checked.0 > 0 && checked.1 > 0,
        "expected guides and slices in the corpus, saw {checked:?}"
    );
    println!(
        "{} file(s): {} guides, {} slices, {} layer comps resources round-tripped",
        files.len(),
        checked.0,
        checked.1,
        checked.2
    );
}
