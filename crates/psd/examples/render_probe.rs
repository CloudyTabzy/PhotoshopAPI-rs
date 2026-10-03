//! Render one or more PSD files to PNG beside the embedded thumbnail comparison.
//! Usage: render_probe <out_dir> <file1.psd> [file2.psd ...]
//!        render_probe --dump <file.psd>   (print layer/mask structure)

use psd::LayeredFile;
use std::path::Path;

fn main() -> psd::core::Result<()> {
    let mut args = std::env::args().skip(1);
    let first = args.next().expect("out_dir or --dump");
    if first == "--dump" {
        let file = LayeredFile::<u8>::read(args.next().unwrap())?;
        dump(&file);
        return Ok(());
    }
    for path in args {
        let file = LayeredFile::<u8>::read(&path)?;
        let image = file.composite_rgba8()?;
        let stem = Path::new(&path)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let out = format!("{first}/{stem}.png");
        image::save_buffer(
            &out,
            &image.rgba,
            image.width,
            image.height,
            image::ColorType::Rgba8,
        )
        .map_err(|e| psd::core::PsdError::Io(std::io::Error::other(e.to_string())))?;
        println!("{stem} {}x{}", image.width, image.height);
    }
    Ok(())
}

fn dump<T: psd::BitDepth>(file: &LayeredFile<T>) {
    fn walk<T: psd::BitDepth>(file: &LayeredFile<T>, parent: Option<psd::LayerId>, depth: usize) {
        let Some(children) = file.children(parent) else {
            return;
        };
        for id in children {
            let Some(layer) = file.layer(*id) else {
                continue;
            };
            let vmask = layer.vector_mask();
            let vmask_str = match &vmask {
                Ok(Some(m)) => format!(
                    "vmsk ok paths={} disabled={} inverted={} ifr={:?}",
                    m.path.records.len(),
                    m.disabled(),
                    m.inverted(),
                    m.path.initial_fill_rule()
                ),
                Ok(None) => "vmsk none".to_string(),
                Err(e) => format!("vmsk ERR {e}"),
            };
            let mask_state = match &layer.mask {
                None => "no-mask".to_string(),
                Some(data) => format!(
                    "pixel_mask={} vector_mask={} mask_rect={:?} key={:?} has_mask={} pixels={:?} pmask_params={:?}",
                    data.pixel_mask.is_some(),
                    data.vector_mask.is_some(),
                    layer.mask_rect(),
                    layer.pixel_mask_key(),
                    layer.has_mask(),
                    layer.mask_pixels().map(|p| p.len()),
                    data.pixel_mask.as_ref().and_then(|r| r.params.as_ref().map(|p| (p.user_mask_density, p.user_mask_feather, p.vector_mask_density, p.vector_mask_feather))),
                ),
            };
            let kind = match &layer.kind {
                psd::layer::LayerKind::SectionDivider(_) => "section",
                psd::layer::LayerKind::Group(_) => "group",
                psd::layer::LayerKind::Image(_) => "image",
                psd::layer::LayerKind::Text(_) => "text",
                psd::layer::LayerKind::Shape(_) => "shape",
                psd::layer::LayerKind::Adjustment(_) => "adjustment",
            };
            println!(
                "{}{:?} [{}] rect={:?} {} {}",
                "  ".repeat(depth),
                layer.name,
                kind,
                layer.bounds,
                vmask_str,
                mask_state,
            );
            if let Some(channels) = layer.channels() {
                for (key, data) in channels.iter() {
                    let stats = (
                        data.len(),
                        data.iter().fold((f32::MAX, f32::MIN), |(lo, hi), v| {
                            let f = v.to_f32();
                            (lo.min(f), hi.max(f))
                        }),
                    );
                    println!(
                        "{}  channel {} len={:?} minmax={:?}",
                        "  ".repeat(depth),
                        key.0,
                        stats.0,
                        stats.1,
                    );
                }
            }
            walk(file, Some(*id), depth + 1);
        }
    }
    walk(file, None, 0);
}
