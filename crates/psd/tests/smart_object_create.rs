//! Smart-object creation, placement transforms, and relinking — the Rust
//! side of upstream `psapi-test/test_smartobjectlayer.py`.
#![cfg(feature = "image")]

use std::path::PathBuf;

use psd::core::{ColorMode, Version};
use psd::{ChannelKey, Homography, Layer, LayerId, LayeredFile, LinkedStorage, Point2, Warp};

fn image_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/python/image_data")
        .join(name)
        .canonicalize()
        .unwrap()
}

fn document_with(storage: LinkedStorage, warp: Option<Warp>) -> (LayeredFile<u8>, LayerId) {
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1024, 1024).unwrap();
    let layer = document
        .create_smart_object(
            "SmartObjectLayer",
            image_path("ImageStackerImage_lowres.png"),
            storage,
            warp,
        )
        .unwrap();
    let id = document.add_layer(layer);
    (document, id)
}

fn size(document: &LayeredFile<u8>, id: LayerId) -> (i32, i32) {
    let bounds = document.layer(id).unwrap().bounds;
    (bounds.width(), bounds.height())
}

fn channels(layer: &Layer<u8>) -> Vec<(ChannelKey, Vec<u8>)> {
    layer
        .channels()
        .unwrap()
        .iter()
        .map(|(key, samples)| (key, samples.to_vec()))
        .collect()
}

#[test]
fn construction_defaults_to_the_source_size() {
    let (document, id) = document_with(LinkedStorage::Embedded, None);
    assert_eq!(size(&document, id), (200, 108));
    let layer = document.layer(id).unwrap();
    assert!(layer.is_smart_object());
    for key in [0, 1, 2, -1] {
        assert_eq!(
            layer
                .channels()
                .unwrap()
                .get(ChannelKey(key))
                .map(<[u8]>::len),
            Some(200 * 108)
        );
    }
    let source = document.smart_object_source(id).unwrap();
    assert_eq!((source.width(), source.height()), (200, 108));
    assert_eq!(
        document.smart_object_storage(id).unwrap(),
        LinkedStorage::Embedded
    );
    assert_eq!(
        document.smart_object_source_path(id).unwrap(),
        Some(image_path("ImageStackerImage_lowres.png"))
    );

    let explicit = Warp::generate_default_with_grid(200, 108, 4, 4).unwrap();
    let (with_warp, id) = document_with(LinkedStorage::Embedded, Some(explicit));
    assert_eq!(size(&with_warp, id), (200, 108));
}

#[test]
fn identical_sources_share_one_link_record() {
    let (mut document, first) = document_with(LinkedStorage::Embedded, None);
    let second = document
        .create_smart_object(
            "Again",
            image_path("ImageStackerImage_lowres.png"),
            LinkedStorage::Embedded,
            None,
        )
        .unwrap();
    let second = document.add_layer(second);
    assert_eq!(document.linked_layer_views().unwrap().len(), 1);
    let identity = |id| {
        document
            .layer(id)
            .unwrap()
            .smart_object_data()
            .unwrap()
            .unwrap()
            .descriptor
            .get("Idnt")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(identity(first), identity(second));
}

#[test]
fn editing_the_warp_re_renders_in_place() {
    let warp = Warp::generate_default_with_grid(200, 108, 4, 4).unwrap();
    let (mut document, id) = document_with(LinkedStorage::Embedded, Some(warp));
    let before = channels(document.layer(id).unwrap());
    document
        .edit_smart_object_warp(id, |warp| {
            let mut points = warp.control_points().to_vec();
            points[0] += Point2::new(25.0, 25.0);
            warp.set_control_points(points)
        })
        .unwrap();
    assert_eq!(size(&document, id), (200, 108));
    let after = channels(document.layer(id).unwrap());
    for ((key, old), (_, new)) in before.iter().zip(&after) {
        assert_ne!(old, new, "channel {key:?} did not change");
    }
    assert!(document.smart_object_warp(id).unwrap().control_points()[0]
        .approx_eq(Point2::new(25.0, 25.0), 1e-9));
}

#[test]
fn placement_transforms_move_resize_and_reset() {
    let (mut document, id) = document_with(LinkedStorage::Embedded, None);
    document
        .move_smart_object(id, Point2::new(50.0, 50.0))
        .unwrap();
    let bounds = document.layer(id).unwrap().bounds;
    assert_eq!((bounds.left, bounds.top), (50, 50));
    assert_eq!(size(&document, id), (200, 108));

    let identity = Homography::identity();
    document.transform_smart_object(id, &identity).unwrap();
    assert_eq!(size(&document, id), (200, 108));

    let center = document
        .smart_object_warp(id)
        .unwrap()
        .bounds()
        .unwrap()
        .center();
    document.rotate_smart_object(id, 45.0, center).unwrap();
    let (width, height) = size(&document, id);
    // A 45° turn of 200×108 spans (200 + 108) / √2 ≈ 218 on both axes.
    assert!(
        (width - 218).abs() <= 2 && (height - 218).abs() <= 2,
        "{width}×{height}"
    );

    document.reset_smart_object_transform(id).unwrap();
    assert_eq!(size(&document, id), (200, 108));
    assert_eq!(document.layer(id).unwrap().bounds.left, 0);

    document
        .scale_smart_object(id, Point2::new(2.0, 2.0), Point2::new(100.0, 54.0))
        .unwrap();
    assert_eq!(size(&document, id), (400, 216));
    document
        .resize_smart_object(id, Some(128.0), Some(128.0))
        .unwrap();
    assert_eq!(size(&document, id), (128, 128));
    document
        .center_smart_object(id, Some(512.0), Some(256.0))
        .unwrap();
    let bounds = document.layer(id).unwrap().bounds;
    assert_eq!((bounds.left, bounds.top), (448, 192));
}

#[test]
fn reset_warp_keeps_the_placement() {
    let (mut document, id) = document_with(LinkedStorage::Embedded, None);
    document
        .edit_smart_object_warp(id, |warp| {
            let mut points = warp.control_points().to_vec();
            points[0] -= Point2::new(50.0, 50.0);
            warp.set_control_points(points)
        })
        .unwrap();
    document
        .move_smart_object(id, Point2::new(10.0, 0.0))
        .unwrap();
    document.reset_smart_object_warp(id).unwrap();
    let warp = document.smart_object_warp(id).unwrap();
    assert!(warp.no_op());
    assert_eq!(size(&document, id), (200, 108));
    assert_eq!(document.layer(id).unwrap().bounds.left, 10);
}

#[test]
fn storage_switches_both_ways_with_a_retained_path() {
    let (mut document, id) = document_with(LinkedStorage::External, None);
    assert_eq!(
        document.smart_object_storage(id).unwrap(),
        LinkedStorage::External
    );
    assert_eq!(
        document.smart_object_source_path(id).unwrap(),
        Some(image_path("ImageStackerImage_lowres.png"))
    );
    document
        .set_smart_object_storage(id, LinkedStorage::Embedded)
        .unwrap();
    assert_eq!(
        document.smart_object_storage(id).unwrap(),
        LinkedStorage::Embedded
    );
    document
        .set_smart_object_storage(id, LinkedStorage::External)
        .unwrap();
    assert_eq!(
        document.smart_object_storage(id).unwrap(),
        LinkedStorage::External
    );

    // An embedded source read back from bytes has no known file.
    let reread = LayeredFile::<u8>::from_bytes(&{
        let (embedded, _) = document_with(LinkedStorage::Embedded, None);
        embedded.to_bytes().unwrap()
    })
    .unwrap();
    let read_id = reread.find_layer("SmartObjectLayer").unwrap();
    assert_eq!(reread.smart_object_source_path(read_id).unwrap(), None);
    let mut reread = reread;
    assert!(reread
        .set_smart_object_storage(read_id, LinkedStorage::External)
        .is_err());
}

#[test]
fn replacement_keeps_the_placement_and_changes_identity() {
    let (mut document, id) = document_with(LinkedStorage::Embedded, None);
    let before = size(&document, id);
    document
        .replace_smart_object(id, image_path("uv_grid.jpg"))
        .unwrap();
    assert_eq!(size(&document, id), before);
    assert_eq!(
        document.smart_object_source_path(id).unwrap(),
        Some(image_path("uv_grid.jpg"))
    );
}

#[test]
fn created_smart_objects_round_trip_in_psd_and_psb() {
    for version in [Version::Psd, Version::Psb] {
        let (mut document, first) = document_with(LinkedStorage::Embedded, None);
        document.version = version;
        let external = document
            .create_smart_object(
                "SmartObjectLayer_external",
                image_path("uv_grid.jpg"),
                LinkedStorage::External,
                None,
            )
            .unwrap();
        let second = document.add_layer(external);
        document
            .resize_smart_object(second, Some(128.0), Some(128.0))
            .unwrap();

        let reread = LayeredFile::<u8>::from_bytes(&document.to_bytes().unwrap()).unwrap();
        for (id, path) in [
            (first, "SmartObjectLayer"),
            (second, "SmartObjectLayer_external"),
        ] {
            let original = document.layer(id).unwrap();
            let read = reread.layer_by_path(path).unwrap();
            assert!(read.is_smart_object());
            assert_eq!(read.bounds, original.bounds);
            assert_eq!(channels(read), channels(original), "{path}");
            assert!(read
                .warp()
                .unwrap()
                .unwrap()
                .approximately_eq(&original.warp().unwrap().unwrap(), 1e-9));
        }
        let read_second = reread.find_layer("SmartObjectLayer_external").unwrap();
        assert_eq!(
            reread.smart_object_storage(read_second).unwrap(),
            LinkedStorage::External
        );
        // The absolute external path resolves without a document path.
        let source = reread.smart_object_source(read_second).unwrap();
        assert!(source.width() > 0);
    }
}

#[test]
fn detached_layers_edit_through_their_home_document_and_graft() {
    let mut home = LayeredFile::<u8>::new(ColorMode::Rgb, 512, 512).unwrap();
    let mut layer = home
        .create_smart_object(
            "Detached",
            image_path("ImageStackerImage_lowres.png"),
            LinkedStorage::Embedded,
            None,
        )
        .unwrap();
    home.edit_smart_object_layer_warp(&mut layer, |warp| warp.resize(Some(128.0), Some(128.0)))
        .unwrap();
    assert_eq!((layer.bounds.width(), layer.bounds.height()), (128, 128));
    assert_eq!(
        home.smart_object_layer_storage(&layer).unwrap(),
        LinkedStorage::Embedded
    );

    let mut other = LayeredFile::<u8>::new(ColorMode::Rgb, 256, 256).unwrap();
    assert!(other.smart_object_source_of(&layer).is_err());
    other.import_smart_object_links(&home, &layer).unwrap();
    other.import_smart_object_links(&home, &layer).unwrap();
    assert_eq!(other.linked_layer_views().unwrap().len(), 1);
    let id = other.add_layer(layer);
    assert_eq!(other.smart_object_source(id).unwrap().width(), 200);
    let reread = LayeredFile::<u8>::from_bytes(&other.to_bytes().unwrap()).unwrap();
    let read_id = reread.find_layer("Detached").unwrap();
    assert_eq!(reread.smart_object_source(read_id).unwrap().height(), 108);
}

#[test]
fn creation_requires_an_rgb_document() {
    let mut document = LayeredFile::<u8>::new(ColorMode::Cmyk, 16, 16).unwrap();
    assert!(document
        .create_smart_object(
            "x",
            image_path("ImageStackerImage_lowres.png"),
            LinkedStorage::Embedded,
            None
        )
        .is_err());
    assert!(document.linked_layer_views().unwrap().is_empty());
}

#[test]
fn rotation_is_in_degrees() {
    let mut warp = Warp::generate_default(100, 50).unwrap();
    warp.rotate(90.0, Point2::new(0.0, 0.0)).unwrap();
    let quad = warp.affine_transform();
    assert!(quad[1].approx_eq(Point2::new(0.0, 100.0), 1e-9), "{quad:?}");
    assert!(quad[2].approx_eq(Point2::new(-50.0, 0.0), 1e-9), "{quad:?}");
}

#[test]
fn missing_source_file_is_rejected_without_touching_the_document() {
    // Upstream "Create layer invalid filepath" (a should-fail case).
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 64, 64).unwrap();
    for storage in [LinkedStorage::Embedded, LinkedStorage::External] {
        assert!(document
            .create_smart_object("SmartObject", PathBuf::from("foo/bar.jpg"), storage, None)
            .is_err());
    }
    assert_eq!(document.layer_count(), 0);
    assert!(document.linked_layers().unwrap().is_empty());
}
