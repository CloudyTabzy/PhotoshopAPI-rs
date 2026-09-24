//! Whole-file parse tests across the entire fixture corpus.
//!
//! This is the Phase 1 "corpus matrix parses" gate: every vendored `.psd` /
//! `.psb` must read as a [`PhotoshopFile`] with consistent layer/channel
//! structure.

use std::path::{Path, PathBuf};

use psd_core::{BeReader, PhotoshopFile};

fn documents_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/documents")
}

fn collect_documents(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_documents(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("psd" | "psb")
        ) {
            out.push(path);
        }
    }
}

#[test]
fn every_fixture_parses_as_photoshop_file() {
    let mut files = Vec::new();
    collect_documents(&documents_dir(), &mut files);
    assert!(
        files.len() >= 70,
        "expected the vendored corpus, found {} files",
        files.len()
    );

    let mut layers_seen = 0usize;
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let mut reader = BeReader::new(&bytes);
        let file =
            PhotoshopFile::read(&mut reader).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

        let info = &file.layer_and_mask_info.layer_info;
        assert_eq!(
            info.layer_records.len(),
            info.channel_image_data.len(),
            "{}: layer/channel count mismatch",
            path.display()
        );
        for (record, channels) in info.layer_records.iter().zip(&info.channel_image_data) {
            assert_eq!(
                record.channels.len(),
                channels.channels.len(),
                "{}: per-layer channel mismatch",
                path.display()
            );
        }
        layers_seen += info.layer_records.len();
    }

    // Sanity: the corpus is a real layer matrix, not empty documents.
    assert!(layers_seen > 200, "only parsed {layers_seen} layers total");
}

#[test]
fn every_fixture_section_prefix_round_trips_byte_exact() {
    let mut files = Vec::new();
    collect_documents(&documents_dir(), &mut files);

    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let mut reader = BeReader::new(&bytes);
        let file = PhotoshopFile::read(&mut reader).unwrap();
        let prefix_len = reader.position();

        let mut writer = psd_core::BeWriter::new();
        file.header.write(&mut writer);
        file.color_mode_data
            .write(&mut writer, &file.header)
            .unwrap();
        file.image_resources.write(&mut writer).unwrap();
        file.layer_and_mask_info
            .write(&mut writer, &file.header)
            .unwrap_or_else(|e| panic!("{}: write failed: {e}", path.display()));

        assert_eq!(
            writer.as_slice(),
            &bytes[..prefix_len],
            "{} did not round-trip byte-exact",
            path.display()
        );
    }
}
