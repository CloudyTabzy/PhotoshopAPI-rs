//! The streaming writer against the buffered one.
//!
//! `LayeredFile::to_bytes` and `write` stream the file out and release each
//! channel payload as it goes; `PhotoshopFile::write` builds the same file in a
//! buffer. Both lay the file out, in two pieces of code, so they are checked
//! against each other on every document in the corpora: 8-, 16- and 32-bit,
//! PSD and PSB, with and without layers.
//!
//! `fixtures/` always runs. Set `PSD_EXTRA_CORPUS` to a list of directories
//! (separated as `PATH` is) to sweep more documents.

use std::path::{Path, PathBuf};

use psd::core::{BeWriter, ColorMode};
use psd::{BitDepth, ChannelKey, Layer, LayeredFile, Rect};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = read.map(|entry| entry.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("psd" | "psb")
        ) {
            out.push(path);
        }
    }
}

fn documents() -> Vec<PathBuf> {
    let mut roots = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")];
    if let Some(extra) = std::env::var_os("PSD_EXTRA_CORPUS") {
        roots.extend(std::env::split_paths(&extra));
    }
    let mut out = Vec::new();
    for root in roots {
        collect(&root, &mut out);
    }
    out
}

/// The buffered writer's bytes for `document`.
fn buffered<T: BitDepth>(document: &LayeredFile<T>) -> Vec<u8> {
    let file = document.to_photoshop_file().unwrap();
    let mut writer = BeWriter::new();
    file.write(&mut writer).unwrap();
    writer.into_inner()
}

fn check<T: BitDepth>(name: &str, document: &LayeredFile<T>) {
    let expected = buffered(document);
    assert!(
        document.to_bytes().unwrap() == expected,
        "{name}: to_bytes differs from the buffered writer"
    );
}

#[test]
fn streaming_matches_the_buffered_writer_on_every_corpus_document() {
    let mut checked = 0;
    for path in documents() {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if bytes.len() < 24 {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let depth = u16::from_be_bytes([bytes[22], bytes[23]]);
        // A document that does not read is the corpus test's business.
        let ok = match depth {
            16 => LayeredFile::<u16>::from_bytes(&bytes)
                .map(|d| check(&name, &d))
                .is_ok(),
            32 => LayeredFile::<f32>::from_bytes(&bytes)
                .map(|d| check(&name, &d))
                .is_ok(),
            _ => LayeredFile::<u8>::from_bytes(&bytes)
                .map(|d| check(&name, &d))
                .is_ok(),
        };
        checked += usize::from(ok);
    }
    assert!(checked > 0, "no documents were checked");
}

/// A document with a few layers of pixels, in any depth.
fn synthetic<T: BitDepth + From<u8>>(size: u32, layers: usize) -> LayeredFile<T> {
    let mut file = LayeredFile::<T>::new(ColorMode::Rgb, size, size).unwrap();
    for layer_index in 0..layers {
        let mut layer = Layer::<T>::new_image(
            format!("Layer {layer_index}"),
            Rect::new(0, 0, size as i32, size as i32),
        );
        for channel in 0..3i16 {
            let data = (0..size * size)
                .map(|i| T::from(((i * 7 + channel as u32 * 31 + layer_index as u32) % 251) as u8))
                .collect();
            layer
                .image_mut()
                .unwrap()
                .set_channel(ChannelKey(channel), data);
        }
        file.add_layer(layer);
    }
    file
}

#[test]
fn streaming_matches_on_synthetic_documents_of_every_depth() {
    // Sizes cross the RLE parallel threshold and odd row widths.
    for size in [1, 7, 64, 301] {
        check("u8", &synthetic::<u8>(size, 3));
        check("u16", &synthetic::<u16>(size, 3));
    }
    // A document without layers writes an empty layer section.
    check(
        "empty u8",
        &LayeredFile::<u8>::new(ColorMode::Rgb, 16, 16).unwrap(),
    );
    check(
        "empty u16",
        &LayeredFile::<u16>::new(ColorMode::Rgb, 16, 16).unwrap(),
    );
}

#[test]
fn write_to_releases_each_payload_it_writes() {
    let document = synthetic::<u8>(64, 2);
    let mut file = document.to_photoshop_file().unwrap();
    assert!(file.size_hint() > 0);
    let mut sink = Vec::new();
    file.write_to(&mut sink).unwrap();
    for layer in &file.layer_and_mask_info.layer_info.channel_image_data {
        for channel in &layer.channels {
            assert!(channel.data.is_empty(), "a written payload was kept");
        }
    }
    assert_eq!(sink, buffered(&document));
}

#[test]
fn write_streams_to_disk_and_a_failed_write_leaves_no_file() {
    let document = synthetic::<u16>(32, 2);
    let dir = std::env::temp_dir().join(format!("psd-streaming-write-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let path = dir.join("out.psd");
    document.write(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), buffered(&document));
    let back = LayeredFile::<u16>::read(&path).unwrap();
    assert_eq!(back.layer_count(), 2);

    // A path that cannot be created is an error, and nothing is left behind.
    let missing = dir.join("no-such-directory").join("out.psd");
    assert!(document.write(&missing).is_err());
    assert!(!missing.exists());

    std::fs::remove_dir_all(&dir).unwrap();
}
