//! Microbenchmarks for the kernels every conversion runs over every texel.
//!
//! Deliberately narrow. `abgen-bench` is what says where a real conversion
//! spends its time; this says whether a change to one of these kernels made
//! that kernel faster, which is a different and much easier question. Adding a
//! bench here is only worth it once `abgen-bench` shows the stage matters.
//!
//! ```sh
//! cargo bench --bench kernels
//! cargo bench --bench kernels -- bc7   # one group
//! ```
//!
//! Inputs are generated, not sampled: a fixed seed makes runs comparable
//! across machines and over time, and the pattern is deliberately awkward —
//! smooth gradients let block encoders take their early-out path and would
//! report a speed no real texture ever sees.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;

/// xorshift, so the corpus is identical everywhere without a dependency.
struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 32) as u32
    }
}

/// Detail at block scale plus a low-frequency wash, which is roughly what an
/// albedo texture looks like to a block encoder: neither flat nor pure noise.
fn texture_rgba(w: usize, h: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let wash = ((x * 255 / w.max(1)) as u32 + (y * 255 / h.max(1)) as u32) / 2;
            let jitter = rng.next_u32() % 64;
            out[i] = ((wash + jitter) % 256) as u8;
            out[i + 1] = ((wash * 2 + jitter) % 256) as u8;
            out[i + 2] = ((wash / 2 + jitter) % 256) as u8;
            out[i + 3] = if (x / 8 + y / 8) % 5 == 0 { 128 } else { 255 };
        }
    }
    out
}

/// BC7 takes its input block-major, 16 RGBA texels per block.
fn to_block_major(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, usize) {
    let (bw, bh) = (w / 4, h / 4);
    let mut out = vec![0u8; bw * bh * 64];
    for by in 0..bh {
        for bx in 0..bw {
            let block = (by * bw + bx) * 64;
            for ty in 0..4 {
                for tx in 0..4 {
                    let src = ((by * 4 + ty) * w + bx * 4 + tx) * 4;
                    let dst = block + (ty * 4 + tx) * 4;
                    out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
                }
            }
        }
    }
    (out, bw * bh)
}

fn bc7(c: &mut Criterion) {
    let mut group = c.benchmark_group("bc7");
    // 256x256 is 4096 blocks: long enough to swamp per-call overhead, short
    // enough that the slow profile finishes in criterion's default budget.
    let (w, h) = (256, 256);
    let (blocks, n) = to_block_major(&texture_rgba(w, h, 0x9E3779B9), w, h);
    group.throughput(Throughput::Bytes((w * h * 4) as u64));

    for (name, params) in [
        ("basic", abgen::bc7_pure::Params::basic(false)),
        ("slow", abgen::bc7_pure::Params::slow(false)),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(name), &params, |b, p| {
            b.iter(|| abgen::bc7_pure::encode_blocks(black_box(&blocks), black_box(n), p))
        });
    }
    group.finish()
}

fn dxt1(c: &mut Criterion) {
    let mut group = c.benchmark_group("dxt1");
    for (w, h) in [(256usize, 256usize), (1024, 1024)] {
        let rgba = texture_rgba(w, h, 0x2545F491);
        group.throughput(Throughput::Bytes((w * h * 4) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{w}x{h}")),
            &rgba,
            |b, src| {
                b.iter(|| {
                    abgen::dxt1_pure::encode_dxt1_mip_chain(
                        black_box(src),
                        w as u32,
                        h as u32,
                        None,
                        false,
                        true,
                    )
                })
            },
        );
    }
    group.finish()
}

fn resize(c: &mut Criterion) {
    let mut group = c.benchmark_group("resize");
    let (w, h) = (2048usize, 2048usize);
    let rgba = texture_rgba(w, h, 0xD1B54A32);
    group.throughput(Throughput::Bytes((w * h * 4) as u64));

    group.bench_function("box_downscale_half", |b| {
        b.iter(|| abgen::resize::box_downscale_rgba(black_box(&rgba), w, h, w / 2, h / 2, true))
    });
    group.bench_function("premul_downscale_half", |b| {
        b.iter(|| abgen::resize::premul_downscale_rgba(black_box(&rgba), w, h, w / 2, h / 2))
    });
    group.finish()
}

criterion_group!(kernels, bc7, dxt1, resize);
criterion_main!(kernels);
