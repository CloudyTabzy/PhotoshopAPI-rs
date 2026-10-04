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

/// The `Lr16`/`Lr32` block of a document, if it carries one.
fn nested_block<T: BitDepth>(document: &LayeredFile<T>, key: [u8; 4]) -> Option<usize> {
    document
        .document_blocks
        .as_ref()?
        .blocks
        .iter()
        .find(|block| block.key.as_bytes() == key)
        .map(|block| block.data.len())
}

#[test]
fn a_read_document_keeps_no_copy_of_its_16_and_32_bit_layer_data() {
    let written = synthetic::<u16>(48, 3).to_bytes().unwrap();
    let document = LayeredFile::<u16>::from_bytes(&written).unwrap();
    // The block stays, which keeps its place among the others, but empty.
    assert_eq!(nested_block(&document, *b"Lr16"), Some(0));
    assert_eq!(document.layer_count(), 3);
    // The layer data is regenerated on write, byte for byte.
    assert_eq!(document.to_bytes().unwrap(), written);

    let written = synthetic::<f32>(16, 2).to_bytes().unwrap();
    let document = LayeredFile::<f32>::from_bytes(&written).unwrap();
    assert_eq!(nested_block(&document, *b"Lr32"), Some(0));
    assert_eq!(document.to_bytes().unwrap(), written);

    // Nothing to regenerate it from once the layers are gone: the empty block is
    // not written, and the file still reads.
    let mut document =
        LayeredFile::<u16>::from_bytes(&synthetic::<u16>(16, 2).to_bytes().unwrap()).unwrap();
    for id in document.root_children().to_vec() {
        document.remove_layer(id).unwrap();
    }
    let bytes = document.to_bytes().unwrap();
    let back = LayeredFile::<u16>::from_bytes(&bytes).unwrap();
    assert_eq!(back.layer_count(), 0);
    assert_eq!(nested_block(&back, *b"Lr16"), None);
}

#[test]
fn a_lazy_document_writes_its_compressed_channels_without_copying_them() {
    use std::borrow::Cow;

    for bytes in [
        synthetic::<u8>(48, 3).to_bytes().unwrap(),
        synthetic::<u16>(48, 3).to_bytes().unwrap(),
    ] {
        let depth = u16::from_be_bytes([bytes[22], bytes[23]]);
        let options = psd::ReadOptions::unlimited().with_raw_data(true);
        let check = |staged: &psd::core::PhotoshopFile<'_>| {
            let mut borrowed = 0;
            for layer in &staged.layer_and_mask_info.layer_info.channel_image_data {
                for channel in &layer.channels {
                    // The divider records a group would add are owned and empty;
                    // every channel with pixels borrows the document's payload.
                    if !channel.data.is_empty() {
                        assert!(matches!(channel.data, Cow::Borrowed(_)));
                        borrowed += 1;
                    }
                }
            }
            assert!(borrowed >= 9, "expected every layer channel to borrow");
        };
        if depth == 16 {
            let document = LayeredFile::<u16>::from_bytes_with_options(&bytes, options).unwrap();
            check(&document.to_photoshop_file().unwrap());
            assert_eq!(document.to_bytes().unwrap(), bytes);
        } else {
            let document = LayeredFile::<u8>::from_bytes_with_options(&bytes, options).unwrap();
            check(&document.to_photoshop_file().unwrap());
            assert_eq!(document.to_bytes().unwrap(), bytes);
        }
    }

    // Encoded payloads own their bytes; native raw byte samples can be
    // borrowed directly without an identity copy.
    let document = synthetic::<u8>(16, 1);
    let staged = document.to_photoshop_file().unwrap();
    for layer in &staged.layer_and_mask_info.layer_info.channel_image_data {
        for channel in &layer.channels {
            if channel.compression == psd::core::Compression::Raw {
                assert!(matches!(channel.data, Cow::Borrowed(_)));
            } else {
                assert!(matches!(channel.data, Cow::Owned(_)));
            }
        }
    }
}

#[test]
fn a_read_document_holds_its_icc_profile_once() {
    let profile: Vec<u8> = (0..2049u32).map(|i| (i % 251) as u8).collect();
    let mut document = synthetic::<u8>(16, 2);
    document.icc_profile = profile.clone();
    let written = document.to_bytes().unwrap();

    let back = LayeredFile::<u8>::from_bytes(&written).unwrap();
    assert_eq!(back.icc_profile, profile);
    // The resource block stays, keeping its place, but does not hold a second copy.
    let block = back.image_resources.icc_profile().expect("the block stays");
    assert!(block.data().is_empty());
    // The save writes the profile back into the block, and reads identically.
    assert_eq!(back.to_bytes().unwrap(), written);

    // Clearing the profile removes the block; setting one adds it back.
    let mut cleared = back;
    cleared.icc_profile.clear();
    let without = cleared.to_bytes().unwrap();
    let reread = LayeredFile::<u8>::from_bytes(&without).unwrap();
    assert!(reread.icc_profile.is_empty());
    assert!(reread.image_resources.icc_profile().is_none());
    cleared.icc_profile = profile.clone();
    assert_eq!(cleared.to_bytes().unwrap(), written);
}

/// `write` replaces the target only once the whole document is on disk. A
/// failure part way must leave whatever was at the path, which matters when a
/// document is saved over its own source.
#[test]
fn write_replaces_the_target_only_after_the_document_is_written() {
    let directory = std::env::temp_dir().join(format!("psd-atomic-write-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("document.psd");
    std::fs::write(&path, b"the original file").unwrap();

    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 4, 4).unwrap();
    document.add_layer(Layer::new_image("Layer", Rect::new(0, 0, 4, 4)));
    document.write(&path).unwrap();
    let written = std::fs::read(&path).unwrap();
    assert_eq!(&written[..4], b"8BPS", "the document replaced the file");

    // No temporary sibling survives a successful write.
    let leftovers: Vec<String> = std::fs::read_dir(&directory)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );

    // A document that cannot serialize leaves the target exactly as it was.
    let mut broken = LayeredFile::<u8>::new(ColorMode::Rgb, 1, 1).unwrap();
    broken.add_layer(Layer::new_image("Invalid", Rect::new(0, 0, -1, -1)));
    assert!(broken.write(&path).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        written,
        "a failed write must not touch the target"
    );

    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn concurrent_saves_use_distinct_temporaries_and_commit_complete_documents() {
    let directory =
        std::env::temp_dir().join(format!("psd-concurrent-write-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("document.psd");
    let documents: Vec<_> = (1u8..=4)
        .map(|value| {
            let mut document = synthetic::<u8>(64, 2);
            for id in document.flatten() {
                let image = document.layer_mut(id).unwrap().image_mut().unwrap();
                image
                    .channels
                    .insert(ChannelKey::color(0), vec![value; 64 * 64]);
            }
            document
        })
        .collect();
    let expected: Vec<_> = documents
        .iter()
        .map(|document| document.to_bytes().unwrap())
        .collect();
    let barrier = std::sync::Barrier::new(documents.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = documents
            .iter()
            .map(|document| {
                let path = &path;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    document.write(path)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    });
    assert!(expected.contains(&std::fs::read(&path).unwrap()));
    // A failed final rename must also remove the temporary owned by that save.
    let blocked = directory.join("directory.psd");
    std::fs::create_dir(&blocked).unwrap();
    assert!(documents[0].write(&blocked).is_err());
    assert!(std::fs::read_dir(&directory).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains(".tmp-")));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn policies_and_workspace_limits_preserve_pixels_and_streamed_layouts() {
    for version in [psd::core::Version::Psd, psd::core::Version::Psb] {
        let mut document = synthetic::<u16>(256, 3);
        document.version = version;
        for policy in [
            psd::CompressionPolicy::Balanced,
            psd::CompressionPolicy::Compact,
            psd::CompressionPolicy::Fast,
        ] {
            let options = psd::WriteOptions::default().with_compression_policy(policy);
            let sequential = document
                .to_bytes_with_options(options.with_working_memory_limit(0))
                .unwrap();
            let bounded = document
                .to_bytes_with_options(options.with_working_memory_limit(512 * 1024))
                .unwrap();
            assert_eq!(sequential, bounded);
            // Growth slack beyond a small margin is trimmed from the result.
            assert!(bounded.capacity() - bounded.len() <= bounded.len() / 32);
            let staged = document.to_photoshop_file_with_options(options).unwrap();
            let mut buffered = psd::core::BeWriter::new();
            staged.write(&mut buffered).unwrap();
            assert_eq!(sequential, buffered.into_inner());
            let reread = LayeredFile::<u16>::from_bytes(&bounded).unwrap();
            for (expected, actual) in document.layers().zip(reread.layers()) {
                for (key, samples) in expected.channels().unwrap().iter() {
                    assert!(
                        Some(samples) == actual.channels().unwrap().get(key),
                        "channel {key:?} changed"
                    );
                }
            }
        }
    }
}

#[test]
fn seekable_writer_checks_callback_counts_and_actual_bytes() {
    let document = synthetic::<u8>(4, 1);
    for wrong_length in [false, true] {
        let mut file = document.to_photoshop_file().unwrap();
        let mut sink = std::io::Cursor::new(Vec::new());
        let result = file.write_seekable(&mut sink, |sink| {
            sink.write_all(&[0; 3])?;
            Ok(if wrong_length {
                vec![vec![100; 4]]
            } else {
                Vec::new()
            })
        });
        assert!(result.is_err());
    }
}

#[test]
fn a_late_codec_failure_keeps_the_target_and_removes_the_private_temporary() {
    let directory =
        std::env::temp_dir().join(format!("psd-late-codec-error-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("document.psd");
    std::fs::write(&path, b"original target").unwrap();
    let mut document = LayeredFile::<f32>::new(ColorMode::Rgb, 30000, 1).unwrap();
    let mut layer = Layer::new_image("Noisy wide row", Rect::new(0, 0, 1, 30000));
    layer.compression = Some(psd::core::Compression::Rle);
    let mut state = 0x1234_5678u32;
    let data = (0..30000)
        .map(|_| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            f32::from_bits(state)
        })
        .collect();
    layer
        .image_mut()
        .unwrap()
        .channels
        .insert(ChannelKey::color(0), data);
    document.add_layer(layer);
    assert!(document.write(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original target");
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn seekable_writer_backpatches_channel_lengths_with_a_constant_number_of_seeks() {
    use std::io::{Cursor, Seek, SeekFrom, Write};
    struct CountingSink {
        inner: Cursor<Vec<u8>>,
        seeks: usize,
    }
    impl Write for CountingSink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.inner.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for CountingSink {
        fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
            self.seeks += 1;
            self.inner.seek(from)
        }
    }

    for layers in [1, 200] {
        let document = synthetic::<u8>(4, layers);
        let staged = document.to_photoshop_file().unwrap();
        let mut buffered = BeWriter::new();
        staged.write(&mut buffered).unwrap();
        let payloads: Vec<Vec<(u16, Vec<u8>)>> = staged
            .layer_and_mask_info
            .layer_info
            .channel_image_data
            .iter()
            .map(|layer| {
                layer
                    .channels
                    .iter()
                    .map(|channel| (channel.compression.as_raw(), channel.data.to_vec()))
                    .collect()
            })
            .collect();
        let mut file = document.to_photoshop_file().unwrap();
        let mut sink = CountingSink {
            inner: Cursor::new(Vec::new()),
            seeks: 0,
        };
        file.write_seekable(&mut sink, |sink| {
            let mut sizes = Vec::new();
            for layer in &payloads {
                let mut layer_sizes = Vec::new();
                for (marker, data) in layer {
                    sink.write_all(&marker.to_be_bytes())?;
                    sink.write_all(data)?;
                    layer_sizes.push(data.len() as u64 + 2);
                }
                sizes.push(layer_sizes);
            }
            Ok(sizes)
        })
        .unwrap();
        assert_eq!(sink.inner.into_inner(), buffered.into_inner());
        // Position query plus two section lengths and one record block, each
        // a seek out and back: independent of the layer count.
        assert!(sink.seeks <= 8, "{layers} layers took {} seeks", sink.seeks);
    }
}
