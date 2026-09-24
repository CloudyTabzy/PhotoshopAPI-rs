//! Editable text layers, following upstream's `docs/doxygen/concepts/text-layers.rst`.
//!
//! Creates a document with a styled text layer, prints its attributes, and
//! optionally edits an existing PSD:
//!
//! ```text
//! cargo run -p psd --example text_layers [existing.psd]
//! ```

use psd::core::ColorMode;
use psd::{
    CharacterDirection, Justification, LayeredFile, Occurrence, TextLayerBuilder,
    TextWritingDirection,
};

fn main() -> psd::core::Result<()> {
    let out_dir = std::env::temp_dir();

    // -- Create a layer ------------------------------------------------------
    let mut document = LayeredFile::<u8>::new(ColorMode::Rgb, 1200, 1800)?;
    let mut layer = TextLayerBuilder::new("Caption", "Hello\nWorld")
        .font("ArialMT")
        .font_size(36.0)
        .fill_color([1.0, 0.0, 0.0, 0.0]) // [A, R, G, B]
        .position(120.0, 160.0)
        .box_size(600.0, 260.0)
        .build::<u8>()?;

    // -- Style / format, high-level first ----------------------------------
    layer
        .style_all()
        .set_font_size(36.0)?
        .set_fill_color([1.0, 0.0, 0.0, 0.0])?;
    layer
        .style_text("World", Occurrence::All)
        .set_underline(true)?;
    layer
        .style_text("World", Occurrence::Nth(0))
        .set_font("Arial-BoldMT")?
        .set_stroke_flag(true)?
        .set_outline_width(2.0)?;
    layer.style_range(0..5).set_underline(true)?;
    layer
        .style_all()
        .set_character_direction(CharacterDirection::LeftToRight)?;
    layer
        .paragraph_all()
        .set_justification(Justification::Center)?;

    // Orientation and text-frame type round trips.
    layer.set_orientation(TextWritingDirection::Vertical)?;
    layer.set_orientation(TextWritingDirection::Horizontal)?;
    layer.convert_to_point_text()?; // box -> point
    layer.convert_to_box_text(600.0, 260.0)?; // point -> box

    // -- Lower-level run control -------------------------------------------
    // "Hello" | "\r" | "World" | "\r" after the range styling above.
    layer.style_run_mut(0).set_faux_bold(true)?;
    layer.split_style_run(2, 3)?; // "Wor" | "ld"
    layer.style_run_mut(3).set_font_size(42.0)?;

    // -- Read attributes ----------------------------------------------------
    println!("text: {:?}", layer.text());
    println!("position: {:?}", layer.text_position());
    println!("box: {:?} x {:?}", layer.box_width(), layer.box_height());
    println!("orientation: {:?}", layer.orientation());
    for (index, run) in layer.style_runs().iter().enumerate() {
        let font = run
            .font_index()
            .and_then(|font| layer.font(font))
            .map(|font| font.postscript_name);
        println!(
            "  run {index}: font={font:?} size={:?} underline={:?} fill={:?}",
            run.font_size(),
            run.underline(),
            run.fill_color()
        );
    }
    for (index, paragraph) in layer.paragraph_runs().iter().enumerate() {
        println!(
            "  paragraph {index}: justification={:?}",
            paragraph.justification()
        );
    }

    document.add_layer(layer);
    let created = out_dir.join("psd_text_layers_created.psd");
    document.write(&created)?;
    println!("wrote {}", created.display());

    // -- Edit an existing PSD -------------------------------------------------
    if let Some(path) = std::env::args().nth(1) {
        let mut existing = LayeredFile::<u8>::read(&path)?;
        let text_layers: Vec<_> = (0..existing.layer_count())
            .filter(|&id| {
                existing
                    .layer(id)
                    .is_some_and(|layer| layer.is_text_layer())
            })
            .collect();
        for id in text_layers {
            let layer = existing.layer_mut(id).expect("layer id from this document");
            if let Some(text) = layer.text() {
                layer.set_text(&text.to_uppercase())?;
            }
        }
        // The stale document-level text cache is dropped on write, so
        // Photoshop offers to update the edited text layers when opening.
        println!("text cache stale: {}", existing.text_cache_is_stale());
        let edited = out_dir.join("psd_text_layers_edited.psd");
        existing.write(&edited)?;
        println!("wrote {}", edited.display());
    }
    Ok(())
}
