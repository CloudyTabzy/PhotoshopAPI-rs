//! Read → decode → write benchmark mirroring the upstream C++ harness:
//! per file, time a raw (structure-only) read, an explicit channel-decode
//! pass, and a write. Compare read + extract: upstream's read already decodes
//! PSD channels into its internally compressed store, whereas our lazy read
//! retains the original streams. Also times
//! the default eager `read`, which combines structure + decode.
//!
//! Usage: cargo run -p psd --release --example rw_bench --features image --
//!          --out <dir> <file1.psd> [file2.psd ...]
//!
//! Prints CSV rows: file,phase,ms with phases read | extract | write |
//! total | eager | pixels.

use psd::core::Result;
use psd::{BitDepth, LayerId, LayeredFile, ReadOptions};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn file_depth(path: &Path) -> u16 {
    let mut hdr = [0u8; 24];
    use std::io::Read;
    std::fs::File::open(path)
        .unwrap()
        .read_exact(&mut hdr)
        .unwrap();
    u16::from_be_bytes([hdr[22], hdr[23]])
}

fn bench_one<T: BitDepth>(input: &Path, out_dir: &Path) -> Result<()> {
    let name = input.file_name().unwrap().to_string_lossy().into_owned();
    let row = |phase: &str, ms: f64| println!("{name},{phase},{ms:.2}");
    let start = Instant::now();

    // Structure-only read (upstream `LayeredFile<T>::read` equivalent).
    let t = Instant::now();
    let mut file =
        LayeredFile::<T>::read_with_options(input, ReadOptions::unlimited().with_raw_data(true))?;
    row("read", t.elapsed().as_secs_f64() * 1e3);

    // Explicit channel decode (upstream per-layer `get_image_data()`).
    let t = Instant::now();
    let mut pixels = 0usize;
    file.decode_all_layer_pixels()?;
    let ids: Vec<LayerId> = file.flatten();
    for id in ids {
        if let Some(layer) = file.layer(id) {
            if let Some(store) = layer.channels() {
                pixels += store
                    .iter()
                    .map(|(_, samples)| samples.len())
                    .sum::<usize>();
            }
        }
    }
    row("extract", t.elapsed().as_secs_f64() * 1e3);

    let out = out_dir.join(format!("rs_{name}"));
    let t = Instant::now();
    file.write(&out)?;
    row("write", t.elapsed().as_secs_f64() * 1e3);
    row("total", start.elapsed().as_secs_f64() * 1e3);

    drop(file);
    let t = Instant::now();
    let eager = LayeredFile::<T>::read(input)?;
    row("eager", t.elapsed().as_secs_f64() * 1e3);
    drop(eager);

    row("pixels", pixels as f64);
    Ok(())
}

fn main() -> Result<()> {
    let mut out_dir = std::env::temp_dir();
    let mut inputs = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--out" {
            out_dir = PathBuf::from(args.next().expect("--out needs a dir"));
        } else {
            inputs.push(PathBuf::from(arg));
        }
    }
    if inputs.is_empty() {
        eprintln!("usage: rw_bench --out <dir> <files...>");
        std::process::exit(1);
    }
    std::fs::create_dir_all(&out_dir).ok();

    for input in &inputs {
        let name = input.file_name().unwrap().to_string_lossy().into_owned();
        let result = match file_depth(input) {
            8 => bench_one::<u8>(input, &out_dir),
            16 => bench_one::<u16>(input, &out_dir),
            32 => bench_one::<f32>(input, &out_dir),
            d => {
                eprintln!("{name}: unsupported depth {d}");
                continue;
            }
        };
        if let Err(e) = result {
            eprintln!("{name}: {e}");
        }
    }
    Ok(())
}
