//! Backend A/B for the ZIP codec's deflate engine: `libdeflater` (C),
//! `zlib-rs` and `miniz_oxide`, measured on real decoded channel planes from
//! the vendored Webtoon corpus (raw RGBA, 9 KB – 1.3 MB — the shape ZIP
//! channel data actually has).
//!
//! Each engine compresses (zlib framing, level 4 — upstream's fixed level)
//! and inflates every input; timing is best-of-N with a correctness gate:
//! every engine must inflate every other engine's stream byte-exactly.
//!
//! Run: `cargo run -p psd-codecs --example zip_bench --release`

use std::path::{Path, PathBuf};
use std::time::Instant;

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/documents/Webtoon")
}

struct Engine {
    name: &'static str,
    compress: fn(&[u8]) -> Vec<u8>,
    decompress: fn(&[u8], usize) -> Vec<u8>,
}

fn libdeflate_compress(data: &[u8]) -> Vec<u8> {
    let mut c = libdeflater::Compressor::new(libdeflater::CompressionLvl::new(4).unwrap());
    let mut out = vec![0u8; c.zlib_compress_bound(data.len())];
    let n = c.zlib_compress(data, &mut out).unwrap();
    out.truncate(n);
    out
}

fn libdeflate_decompress(data: &[u8], out_len: usize) -> Vec<u8> {
    let mut d = libdeflater::Decompressor::new();
    let mut out = vec![0u8; out_len];
    let n = d.zlib_decompress(data, &mut out).unwrap();
    out.truncate(n);
    out
}

fn zlib_rs_compress(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; zlib_rs::compress_bound(data.len())];
    let (written, rc) = zlib_rs::compress_slice(&mut out, data, zlib_rs::DeflateConfig::new(4));
    let len = written.len();
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    out.truncate(len);
    out
}

fn zlib_rs_decompress(data: &[u8], out_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; out_len];
    let (written, rc) =
        zlib_rs::decompress_slice(&mut out, data, zlib_rs::InflateConfig::default());
    let len = written.len();
    assert_eq!(rc, zlib_rs::ReturnCode::Ok);
    out.truncate(len);
    out
}

fn miniz_compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(data, 4)
}

fn miniz_decompress(data: &[u8], out_len: usize) -> Vec<u8> {
    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, out_len).unwrap()
}

fn main() {
    let engines = [
        Engine {
            name: "libdeflater (C)",
            compress: libdeflate_compress,
            decompress: libdeflate_decompress,
        },
        Engine {
            name: "zlib-rs",
            compress: zlib_rs_compress,
            decompress: zlib_rs_decompress,
        },
        Engine {
            name: "miniz_oxide",
            compress: miniz_compress,
            decompress: miniz_decompress,
        },
    ];

    let mut inputs: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in std::fs::read_dir(corpus_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("example.layer") || name == "example.imageData" {
            inputs.push((name, std::fs::read(&path).unwrap()));
        }
    }
    // Two synthetic extremes alongside the real planes: a deterministic
    // incompressible stream (worst case for every engine) and a smooth
    // photographic gradient (long matches, few literals).
    let mut noise = Vec::with_capacity(1 << 22);
    let mut state = 0x12345678u64;
    while noise.len() < (1 << 22) {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        noise.push((state >> 33) as u8);
    }
    inputs.push(("synthetic-noise".to_owned(), noise));
    let gradient: Vec<u8> = (0..(1 << 22))
        .map(|i| ((i % 1024) * 255 / 1023 + (i / 1024 % 251)) as u8)
        .collect();
    inputs.push(("synthetic-gradient".to_owned(), gradient));

    inputs.sort_by_key(|(_, data)| data.len());
    let total_in: usize = inputs.iter().map(|(_, d)| d.len()).sum();
    println!(
        "{} inputs, {:.2} MiB total",
        inputs.len(),
        total_in as f64 / 1048576.0
    );

    // Correctness gate: every engine inflates every other engine's stream.
    for (name, data) in &inputs {
        for enc in &engines {
            let compressed = (enc.compress)(data);
            for dec in &engines {
                let back = (dec.decompress)(&compressed, data.len());
                assert_eq!(&back, data, "{name}: {} -> {}", enc.name, dec.name);
            }
        }
    }
    println!("cross-engine round-trip: all pairs byte-exact\n");

    for engine in &engines {
        let (mut comp_ns, mut decomp_ns, mut comp_bytes, mut decomp_bytes) =
            (0u128, 0u128, 0usize, 0usize);
        let mut total_comp_len = 0usize;
        for (_, data) in &inputs {
            // Best-of up to 50 iterations per file (fewer for large inputs).
            let iters = ((200usize << 20) / data.len()).clamp(5, 50);
            let compressed = (engine.compress)(data);
            total_comp_len += compressed.len();
            let mut best_c = u128::MAX;
            for _ in 0..iters {
                let t = Instant::now();
                std::hint::black_box((engine.compress)(std::hint::black_box(data)));
                best_c = best_c.min(t.elapsed().as_nanos());
            }
            comp_ns += best_c;
            comp_bytes += data.len();
            let mut best_d = u128::MAX;
            for _ in 0..iters {
                let t = Instant::now();
                std::hint::black_box((engine.decompress)(
                    std::hint::black_box(&compressed),
                    data.len(),
                ));
                best_d = best_d.min(t.elapsed().as_nanos());
            }
            decomp_ns += best_d;
            decomp_bytes += data.len();
        }
        let c_mbs = comp_bytes as f64 / (comp_ns as f64 / 1e9) / 1048576.0;
        let d_mbs = decomp_bytes as f64 / (decomp_ns as f64 / 1e9) / 1048576.0;
        let ratio = total_comp_len as f64 / comp_bytes as f64;
        println!(
            "{:16}  compress {:7.1} MiB/s   decompress {:7.1} MiB/s   ratio {:.4}",
            engine.name, c_mbs, d_mbs, ratio
        );
    }

    // Per-input detail on the representative cases: the aggregate above is
    // dominated by the supercompressible planes.
    println!();
    for (name, data) in inputs
        .iter()
        .filter(|(n, d)| n.starts_with("synthetic") || d.len() > 1 << 20)
    {
        println!("{name} ({:.1} MiB):", data.len() as f64 / 1048576.0);
        for engine in &engines {
            let compressed = (engine.compress)(data);
            let mut best_c = u128::MAX;
            for _ in 0..30 {
                let t = Instant::now();
                std::hint::black_box((engine.compress)(std::hint::black_box(data)));
                best_c = best_c.min(t.elapsed().as_nanos());
            }
            let mut best_d = u128::MAX;
            for _ in 0..30 {
                let t = Instant::now();
                std::hint::black_box((engine.decompress)(
                    std::hint::black_box(&compressed),
                    data.len(),
                ));
                best_d = best_d.min(t.elapsed().as_nanos());
            }
            let mbs = |ns| data.len() as f64 / (ns as f64 / 1e9) / 1048576.0;
            println!(
                "  {:16}  compress {:7.1} MiB/s   decompress {:7.1} MiB/s   size {:.4}",
                engine.name,
                mbs(best_c),
                mbs(best_d),
                compressed.len() as f64 / data.len() as f64
            );
        }
    }
}
