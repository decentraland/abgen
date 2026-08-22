use crate::gpu::corelib::bc7::{build_opt_tables, Bc7Profile, EndpointErr, OptTables, Params};
use crate::gpu::corelib::mips::{box_halve_dims, compute_default_mip_count, level_block_dims};
#[cfg(not(target_arch = "wasm32"))]
use crate::gpu::wgpu::gpu;
use crate::gpu::wgpu::{Gpu, BLOCKIFY_WGSL};
use anyhow::{anyhow, ensure, Result};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::OnceLock;

pub(crate) const BC7_WGSL: &str = include_str!("../shaders/bc7.wgsl");

pub(crate) const PARAMS_WORDS: usize = 42;
pub(crate) const OPT_TABLES_WORDS: usize = 4352;
pub(crate) const PLAN_STRIDE: usize = 110;

const _: () = assert!(std::mem::size_of::<Params>() == 124);
const _: () = assert!(std::mem::align_of::<Params>() == 4);
const _: () = assert!(std::mem::size_of::<EndpointErr>() == 4);
const _: () = assert!(std::mem::align_of::<EndpointErr>() == 2);
const _: () = assert!(std::mem::size_of::<OptTables>() == OPT_TABLES_WORDS * 4);
const _: () = assert!(std::mem::align_of::<OptTables>() == 4);

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn endpoint_err_word(e: EndpointErr) -> u32 {
    e.error as u32 | ((e.lo as u32) << 16) | ((e.hi as u32) << 24)
}

pub(crate) fn params_words(p: &Params) -> Vec<u32> {
    let mut w = Vec::with_capacity(PARAMS_WORDS);
    w.extend_from_slice(&p.max_partitions_mode);
    w.extend_from_slice(&p.weights);
    w.push(p.uber_level);
    w.push(p.refinement_passes);
    w.push(p.mode4_rotation_mask);
    w.push(p.mode4_index_mask);
    w.push(p.mode5_rotation_mask);
    w.push(p.uber1_mask);
    w.push(p.perceptual as u32);
    w.push(p.pbit_search as u32);
    w.push(p.mode6_only as u32);
    w.push(p.op_max_mode13);
    w.push(p.op_max_mode0);
    w.push(p.op_max_mode2);
    for b in p.use_mode {
        w.push(b as u32);
    }
    w.push(p.al_max_mode7);
    w.extend_from_slice(&p.mode67_weight_mul);
    w.push(p.use_mode4 as u32);
    w.push(p.use_mode5 as u32);
    w.push(p.use_mode6 as u32);
    w.push(p.use_mode7 as u32);
    w.push(p.use_mode4_rotation as u32);
    w.push(p.use_mode5_rotation as u32);
    assert_eq!(w.len(), PARAMS_WORDS);
    w
}

pub(crate) fn opt_tables_words(t: &OptTables) -> Vec<u32> {
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (t as *const OptTables).cast::<u8>(),
            std::mem::size_of::<OptTables>(),
        )
    };
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub(crate) fn words_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

pub struct Engine {
    lin: ::wgpu::ComputePipeline,
    halve: ::wgpu::ComputePipeline,
    pack: ::wgpu::ComputePipeline,
    plan: [::wgpu::ComputePipeline; 3],
    enc: [::wgpu::ComputePipeline; 3],
    opt: ::wgpu::Buffer,
    params: [::wgpu::Buffer; 4],
    zero_prefix: ::wgpu::Buffer,
    pools: Pools,
}

const META_BYTES: u64 = 16;
const POOL_PER_CLASS: usize = 4;
const STORAGE_POOL_USAGES: ::wgpu::BufferUsages = ::wgpu::BufferUsages::STORAGE
    .union(::wgpu::BufferUsages::COPY_DST)
    .union(::wgpu::BufferUsages::COPY_SRC);

// Reuse pools keyed by size class. Cross-submission reuse is safe: all work goes
// through the one queue, so later write_buffer/dispatches are ordered after
// earlier reads. Buffers are recycled with stale contents; the only consumer
// that relied on zero-init is the plan scratch, cleared in record_encode.
#[derive(Default)]
struct Pools {
    storage: Mutex<HashMap<u64, Vec<::wgpu::Buffer>>>,
    uniform: Mutex<Vec<::wgpu::Buffer>>,
    readback: Mutex<HashMap<u64, Vec<::wgpu::Buffer>>>,
}

// Round up with <=1/16 slack so nearby sizes share a pool entry.
fn size_class(bytes: u64) -> u64 {
    let n = bytes.max(256);
    let g = (n.next_power_of_two() / 16).max(256);
    n.div_ceil(g) * g
}

impl Engine {
    fn take_storage(&self, g: &Gpu, bytes: u64) -> ::wgpu::Buffer {
        let class = size_class(bytes);
        let pooled = self
            .pools
            .storage
            .lock()
            .unwrap()
            .get_mut(&class)
            .and_then(Vec::pop);
        pooled.unwrap_or_else(|| {
            g.device.create_buffer(&::wgpu::BufferDescriptor {
                label: Some("pool-storage"),
                size: class,
                usage: STORAGE_POOL_USAGES,
                mapped_at_creation: false,
            })
        })
    }

    fn put_storage(&self, b: ::wgpu::Buffer) {
        let mut pool = self.pools.storage.lock().unwrap();
        let v = pool.entry(b.size()).or_default();
        if v.len() < POOL_PER_CLASS {
            v.push(b);
        }
    }

    fn take_meta(&self, g: &Gpu) -> ::wgpu::Buffer {
        let pooled = self.pools.uniform.lock().unwrap().pop();
        pooled.unwrap_or_else(|| {
            g.device.create_buffer(&::wgpu::BufferDescriptor {
                label: Some("meta"),
                size: META_BYTES,
                usage: ::wgpu::BufferUsages::UNIFORM | ::wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        })
    }

    fn put_metas(&self, bufs: Vec<::wgpu::Buffer>) {
        let mut pool = self.pools.uniform.lock().unwrap();
        for b in bufs {
            if pool.len() < 64 {
                pool.push(b);
            }
        }
    }

    fn take_readback(&self, g: &Gpu, bytes: u64) -> ::wgpu::Buffer {
        let class = size_class(bytes);
        let pooled = self
            .pools
            .readback
            .lock()
            .unwrap()
            .get_mut(&class)
            .and_then(Vec::pop);
        pooled.unwrap_or_else(|| {
            g.device.create_buffer(&::wgpu::BufferDescriptor {
                label: Some("bc7-staging"),
                size: class,
                usage: ::wgpu::BufferUsages::MAP_READ | ::wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        })
    }

    fn put_readback(&self, b: ::wgpu::Buffer) {
        let mut pool = self.pools.readback.lock().unwrap();
        let v = pool.entry(b.size()).or_default();
        if v.len() < POOL_PER_CLASS {
            v.push(b);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
static ENGINE: OnceLock<Engine> = OnceLock::new();

fn make_pipeline(
    g: &Gpu,
    module: &::wgpu::ShaderModule,
    entry: &str,
    constants: &[(&str, f64)],
) -> ::wgpu::ComputePipeline {
    g.device
        .create_compute_pipeline(&::wgpu::ComputePipelineDescriptor {
            label: Some(entry),
            layout: None,
            module,
            entry_point: Some(entry),
            compilation_options: ::wgpu::PipelineCompilationOptions {
                constants,
                ..Default::default()
            },
            cache: None,
        })
}

fn storage_init(g: &Gpu, label: &str, data: &[u8]) -> ::wgpu::Buffer {
    use ::wgpu::util::DeviceExt;
    g.device
        .create_buffer_init(&::wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: data,
            usage: ::wgpu::BufferUsages::STORAGE,
        })
}

pub fn build_engine(g: &Gpu) -> Engine {
    let blockify = g
        .device
        .create_shader_module(::wgpu::ShaderModuleDescriptor {
            label: Some("blockify"),
            source: ::wgpu::ShaderSource::Wgsl(BLOCKIFY_WGSL.into()),
        });
    let bc7 = g
        .device
        .create_shader_module(::wgpu::ShaderModuleDescriptor {
            label: Some("bc7"),
            source: ::wgpu::ShaderSource::Wgsl(BC7_WGSL.into()),
        });
    let t = build_opt_tables();
    let opt = storage_init(g, "bc7-opt-tables", &words_bytes(&opt_tables_words(&t)));
    let params = [
        Params::slow(false),
        Params::slow(true),
        Params::basic(false),
        Params::basic(true),
    ]
    .map(|p| storage_init(g, "bc7-params", &words_bytes(&params_words(&p))));
    Engine {
        lin: make_pipeline(g, &blockify, "blockify_linearize", &[]),
        halve: make_pipeline(g, &blockify, "blockify_halve", &[]),
        pack: make_pipeline(g, &blockify, "blockify_quantize_pack", &[]),
        plan: [
            make_pipeline(g, &bc7, "bc7_plan_alpha", &[]),
            make_pipeline(g, &bc7, "bc7_plan_opaque13", &[]),
            make_pipeline(g, &bc7, "bc7_plan_opaque02", &[]),
        ],
        enc: [0.0f64, 1.0, 2.0]
            .map(|c| make_pipeline(g, &bc7, "bc7_encode_blocks", &[("TRIAL_CLASS", c)])),
        opt,
        params,
        zero_prefix: storage_init(g, "prefix0", &0u64.to_le_bytes()),
        pools: Pools::default(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn engine(g: &'static Gpu) -> &'static Engine {
    ENGINE.get_or_init(|| build_engine(g))
}

fn push_u64(b: &mut Vec<u8>, x: u64) {
    b.extend_from_slice(&x.to_le_bytes());
}

fn push_u32(b: &mut Vec<u8>, x: u32) {
    b.extend_from_slice(&x.to_le_bytes());
}

fn lin_item_bytes(base_px: u64, pyr_px: u64, srgb: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    push_u64(&mut b, base_px);
    push_u64(&mut b, pyr_px);
    push_u32(&mut b, srgb as u32);
    push_u32(&mut b, 0);
    b
}

fn halve_item_bytes(src_px: u64, dst_px: u64, w: u32, h: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(24);
    push_u64(&mut b, src_px);
    push_u64(&mut b, dst_px);
    push_u32(&mut b, w);
    push_u32(&mut b, h);
    b
}

fn pack_item_bytes(lvl_px: u64, blk_off: u64, w: u32, h: u32, srgb: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    push_u64(&mut b, lvl_px);
    push_u64(&mut b, blk_off);
    push_u32(&mut b, w);
    push_u32(&mut b, h);
    push_u32(&mut b, srgb as u32);
    push_u32(&mut b, 0);
    b
}

fn flip_rgba(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut flipped = vec![0u8; w * h * 4];
    for y in 0..h {
        let src = &rgba[(h - 1 - y) * w * 4..(h - y) * w * 4];
        flipped[y * w * 4..(y + 1) * w * 4].copy_from_slice(src);
    }
    flipped
}

struct Stage<'a> {
    pipeline: &'a ::wgpu::ComputePipeline,
    wg: u32,
    // gid range [first, total): total is the exclusive end, as the shader guard
    // is `gid >= job.total`.
    first: u64,
    total: u64,
    n_items: u32,
    fone: bool,
    // (binding, buffer, bound bytes) — pooled buffers are class-sized, so each
    // binding carries its exact logical size.
    bufs: &'a [(u32, &'a ::wgpu::Buffer, u64)],
}

fn run_stage(
    g: &Gpu,
    eng: &Engine,
    enc: &mut ::wgpu::CommandEncoder,
    metas: &mut Vec<::wgpu::Buffer>,
    st: &Stage,
) {
    let max_wg = g.device.limits().max_compute_workgroups_per_dimension as u64;
    let chunk = max_wg * st.wg as u64;
    let layout = st.pipeline.get_bind_group_layout(0);
    let mut base = st.first;
    while base < st.total {
        let n = (st.total - base).min(chunk);
        let fone = if st.fone { 1.0f32.to_bits() } else { 0 };
        let mut meta = Vec::with_capacity(16);
        for v in [st.n_items, st.total as u32, base as u32, fone] {
            meta.extend_from_slice(&v.to_le_bytes());
        }
        // One pooled meta buffer per dispatch: everything is recorded into a
        // single submit, so a shared buffer could not hold per-stage values.
        let meta_buf = eng.take_meta(g);
        g.queue.write_buffer(&meta_buf, 0, &meta);
        let bind = |buf, size| {
            ::wgpu::BindingResource::Buffer(::wgpu::BufferBinding {
                buffer: buf,
                offset: 0,
                size: std::num::NonZeroU64::new(size),
            })
        };
        let mut entries = vec![::wgpu::BindGroupEntry {
            binding: 0,
            resource: bind(&meta_buf, META_BYTES),
        }];
        for (binding, buf, size) in st.bufs {
            entries.push(::wgpu::BindGroupEntry {
                binding: *binding,
                resource: bind(buf, *size),
            });
        }
        let bg = g.device.create_bind_group(&::wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &entries,
        });
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(st.pipeline);
        pass.set_bind_group(0, &bg, &[]);
        pass.dispatch_workgroups(n.div_ceil(st.wg as u64) as u32, 1, 1);
        drop(pass);
        metas.push(meta_buf);
        base += n;
    }
}

struct Level {
    w: usize,
    h: usize,
    px_off: u64,
    blk_off: u64,
    nb: u64,
}

pub(crate) fn buffer_demand(
    base_bytes: u64,
    total_px: u64,
    nb0: u64,
    num_blocks: u64,
) -> (u64, u64) {
    let binding = base_bytes
        .max(total_px * 16)
        .max(nb0 * 64)
        .max(nb0 * PLAN_STRIDE as u64 * 4);
    (binding, binding.max(num_blocks * 16))
}

#[allow(clippy::too_many_arguments)]
fn record_encode(
    g: &Gpu,
    eng: &Engine,
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    srgb: bool,
    perceptual: bool,
    profile: Bc7Profile,
) -> Result<(::wgpu::Buffer, u64, i32)> {
    let w = width as usize;
    let h = height as usize;
    ensure!(width > 0 && height > 0, "empty texture {width}x{height}");
    ensure!(
        rgba.len() == w * h * 4,
        "rgba len {} != {}x{}x4",
        rgba.len(),
        w,
        h
    );
    let mips = mip_count.unwrap_or_else(|| compute_default_mip_count(width, height));
    ensure!(mips >= 1, "mip_count {mips} < 1");
    let bucket = match (profile, perceptual) {
        (Bc7Profile::Slow, false) => 0usize,
        (Bc7Profile::Slow, true) => 1,
        (Bc7Profile::Basic, false) => 2,
        (Bc7Profile::Basic, true) => 3,
    };
    let data: Cow<[u8]> = if flip {
        Cow::Owned(flip_rgba(rgba, width, height))
    } else {
        Cow::Borrowed(rgba)
    };
    let mut levels = Vec::with_capacity(mips as usize);
    let (mut cw, mut ch) = (w, h);
    let (mut px_off, mut blk_off) = (0u64, 0u64);
    for _ in 0..mips {
        let (bw, bh) = level_block_dims(cw, ch);
        levels.push(Level {
            w: cw,
            h: ch,
            px_off,
            blk_off,
            nb: (bw * bh) as u64,
        });
        px_off += (cw * ch) as u64;
        blk_off += (bw * bh) as u64;
        let (nw, nh) = box_halve_dims(cw, ch);
        cw = nw;
        ch = nh;
    }
    let total_px = px_off;
    let num_blocks = blk_off;
    let limits = g.device.limits();
    let (need_binding, need_buffer) =
        buffer_demand(data.len() as u64, total_px, levels[0].nb, num_blocks);
    ensure!(
        need_binding <= limits.max_storage_buffer_binding_size
            && need_buffer <= limits.max_buffer_size,
        "texture {width}x{height} mips={mips} exceeds wgpu device limits: needs storage binding {need_binding} B (max {}) and buffer {need_buffer} B (max {})",
        limits.max_storage_buffer_binding_size,
        limits.max_buffer_size
    );
    // Whole-chain batching: one pooled blocks/scratch/out buffer set covers as
    // many consecutive levels as the device limits allow (normally all of them);
    // the greedy split only engages near the limits, and any single level fits
    // by the ensure above. Level starts are padded to the 4-block plan group so
    // the plan passes keep bc7_pure's per-level group composition (group results
    // are not lane-independent; cross-level groups change bytes).
    let pad4 = |nb: u64| nb.next_multiple_of(4);
    let fits = |nb: u64| {
        let need = (nb * 64).max(nb * PLAN_STRIDE as u64 * 4);
        need <= limits.max_storage_buffer_binding_size && need <= limits.max_buffer_size
    };
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut acc = pad4(levels[0].nb);
    for (i, l) in levels.iter().enumerate().skip(1) {
        if fits(acc + pad4(l.nb)) {
            acc += pad4(l.nb);
        } else {
            segs.push((start, i));
            start = i;
            acc = pad4(l.nb);
        }
    }
    segs.push((start, levels.len()));
    // Padded per-level block offset within its segment's buffers.
    let mut poffs = vec![0u64; levels.len()];
    for &(s, e) in &segs {
        let mut off = 0u64;
        for i in s..e {
            poffs[i] = off;
            off += pad4(levels[i].nb);
        }
    }
    let base_buf = eng.take_storage(g, data.len() as u64);
    g.queue.write_buffer(&base_buf, 0, &data);
    let pyr_bytes = total_px * 16;
    let pyr_buf = eng.take_storage(g, pyr_bytes);
    let out_len = num_blocks * 16;
    let staging = eng.take_readback(g, out_len);
    let lin_items = eng.take_storage(g, 24);
    g.queue
        .write_buffer(&lin_items, 0, &lin_item_bytes(0, 0, srgb));
    let halve_bufs = if mips > 1 {
        let mut items = Vec::with_capacity((mips as usize - 1) * 24);
        let mut prefixes = Vec::with_capacity((mips as usize - 1) * 8);
        for i in 1..levels.len() {
            let (src, dst) = (&levels[i - 1], &levels[i]);
            items.extend_from_slice(&halve_item_bytes(
                src.px_off,
                dst.px_off,
                src.w as u32,
                src.h as u32,
            ));
            // gid space for halve is the dst pixel's pyramid offset.
            prefixes.extend_from_slice(&dst.px_off.to_le_bytes());
        }
        let items_buf = eng.take_storage(g, items.len() as u64);
        g.queue.write_buffer(&items_buf, 0, &items);
        let prefix_buf = eng.take_storage(g, prefixes.len() as u64);
        g.queue.write_buffer(&prefix_buf, 0, &prefixes);
        Some((
            items_buf,
            items.len() as u64,
            prefix_buf,
            prefixes.len() as u64,
        ))
    } else {
        None
    };
    let mut pack_items = Vec::with_capacity(levels.len() * 32);
    let mut pack_prefixes = Vec::with_capacity(levels.len() * 8);
    for (l, &poff) in levels.iter().zip(&poffs) {
        // blk_off is the padded offset into the segment's blocks buffer; the
        // gid space (and prefixes) stay global and unpadded.
        pack_items.extend_from_slice(&pack_item_bytes(
            l.px_off, poff, l.w as u32, l.h as u32, srgb,
        ));
        pack_prefixes.extend_from_slice(&l.blk_off.to_le_bytes());
    }
    let pack_items_buf = eng.take_storage(g, pack_items.len() as u64);
    g.queue.write_buffer(&pack_items_buf, 0, &pack_items);
    let pack_prefix_buf = eng.take_storage(g, pack_prefixes.len() as u64);
    g.queue.write_buffer(&pack_prefix_buf, 0, &pack_prefixes);
    let params_buf = &eng.params[bucket];
    let mut metas: Vec<::wgpu::Buffer> = Vec::new();
    let mut recycle: Vec<::wgpu::Buffer> = Vec::new();
    let mut cmd = g.device.create_command_encoder(&Default::default());
    run_stage(
        g,
        eng,
        &mut cmd,
        &mut metas,
        &Stage {
            pipeline: &eng.lin,
            wg: 256,
            first: 0,
            total: (levels[0].w * levels[0].h) as u64,
            n_items: 1,
            fone: false,
            bufs: &[
                (1, &lin_items, 24),
                (4, &eng.zero_prefix, 8),
                (5, &base_buf, data.len() as u64),
                (6, &pyr_buf, pyr_bytes),
            ],
        },
    );
    if let Some((items_buf, items_len, prefix_buf, prefix_len)) = &halve_bufs {
        for dst in &levels[1..] {
            run_stage(
                g,
                eng,
                &mut cmd,
                &mut metas,
                &Stage {
                    pipeline: &eng.halve,
                    wg: 256,
                    first: dst.px_off,
                    total: dst.px_off + (dst.w * dst.h) as u64,
                    n_items: mips as u32 - 1,
                    fone: false,
                    bufs: &[
                        (3, items_buf, *items_len),
                        (4, prefix_buf, *prefix_len),
                        (6, &pyr_buf, pyr_bytes),
                    ],
                },
            );
        }
    }
    for &(s, e) in &segs {
        let seg_pnb: u64 = levels[s..e].iter().map(|l| pad4(l.nb)).sum();
        let seg_nb_end = levels[e - 1].blk_off + levels[e - 1].nb;
        let blocks_bytes = seg_pnb * 64;
        let scratch_bytes = seg_pnb * PLAN_STRIDE as u64 * 4;
        let seg_out_bytes = seg_pnb * 16;
        let blocks = eng.take_storage(g, blocks_bytes);
        let scratch = eng.take_storage(g, scratch_bytes);
        let out = eng.take_storage(g, seg_out_bytes);
        // Pooled scratch carries stale plan flags (the plan passes only write
        // fields for their class); restore the fresh-buffer zero contract.
        cmd.clear_buffer(&scratch, 0, Some(scratch_bytes));
        run_stage(
            g,
            eng,
            &mut cmd,
            &mut metas,
            &Stage {
                pipeline: &eng.pack,
                wg: 256,
                first: levels[s].blk_off,
                total: seg_nb_end,
                n_items: levels.len() as u32,
                fone: false,
                bufs: &[
                    (2, &pack_items_buf, pack_items.len() as u64),
                    (4, &pack_prefix_buf, pack_prefixes.len() as u64),
                    (6, &pyr_buf, pyr_bytes),
                    (7, &blocks, blocks_bytes),
                ],
            },
        );
        for i in s..e {
            let (nb, poff) = (levels[i].nb, poffs[i]);
            for pipe in &eng.plan {
                run_stage(
                    g,
                    eng,
                    &mut cmd,
                    &mut metas,
                    &Stage {
                        pipeline: pipe,
                        wg: 64,
                        first: poff / 4,
                        total: poff / 4 + nb.div_ceil(4),
                        n_items: (poff + nb) as u32,
                        fone: true,
                        bufs: &[
                            (1, params_buf, params_buf.size()),
                            (4, &blocks, blocks_bytes),
                            (3, &scratch, scratch_bytes),
                        ],
                    },
                );
            }
        }
        for i in s..e {
            let (nb, poff) = (levels[i].nb, poffs[i]);
            for pipe in &eng.enc {
                run_stage(
                    g,
                    eng,
                    &mut cmd,
                    &mut metas,
                    &Stage {
                        pipeline: pipe,
                        wg: 64,
                        first: poff,
                        total: poff + nb,
                        n_items: (poff + nb) as u32,
                        fone: true,
                        bufs: &[
                            (1, params_buf, params_buf.size()),
                            (2, &eng.opt, eng.opt.size()),
                            (4, &blocks, blocks_bytes),
                            (5, &scratch, scratch_bytes),
                            (3, &out, seg_out_bytes),
                        ],
                    },
                );
            }
            cmd.copy_buffer_to_buffer(&out, poff * 16, &staging, levels[i].blk_off * 16, nb * 16);
        }
        recycle.extend([blocks, scratch, out]);
    }
    g.queue.submit([cmd.finish()]);
    recycle.extend([
        base_buf,
        pyr_buf,
        lin_items,
        pack_items_buf,
        pack_prefix_buf,
    ]);
    if let Some((items_buf, _, prefix_buf, _)) = halve_bufs {
        recycle.extend([items_buf, prefix_buf]);
    }
    for b in recycle {
        eng.put_storage(b);
    }
    eng.put_metas(metas);
    Ok((staging, out_len, mips))
}

// Blocking wait + map + copy + release for one texture's staging buffer. Split
// out of encode_bc7_mip_chain so encode_bc7_mip_chain_batch can submit texture
// N's dispatch (record_encode, which ends in a non-blocking queue.submit)
// before waiting on texture N-1's readback: N's command buffer is already
// queued behind N-1's by the time this call blocks, so the GPU keeps N-1's
// tail and N's dispatch running back-to-back instead of idling on the CPU's
// per-texture record/submit gap. wgpu's single-queue timeline still makes
// this byte-identical to the fully sequential path — command buffers execute,
// and any resource they touch is synchronized, in submission order.
#[cfg(not(target_arch = "wasm32"))]
fn blocking_readback(
    g: &Gpu,
    eng: &Engine,
    staging: ::wgpu::Buffer,
    out_len: u64,
    mips: i32,
) -> Result<(Vec<u8>, i32)> {
    let slice = staging.slice(0..out_len);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(::wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    g.device
        .poll(::wgpu::PollType::wait_indefinitely())
        .map_err(|e| anyhow!("wgpu device poll failed: {e:?}"))?;
    rx.recv()
        .map_err(|_| anyhow!("wgpu map_async callback dropped"))?
        .map_err(|e| anyhow!("wgpu readback map failed: {e:?}"))?;
    let out = slice
        .get_mapped_range()
        .map_err(|e| anyhow!("wgpu mapped range failed: {e:?}"))?
        .to_vec();
    staging.unmap();
    eng.put_readback(staging);
    Ok((out, mips))
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_bc7_mip_chain(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    srgb: bool,
    perceptual: bool,
    profile: Bc7Profile,
) -> Result<(Vec<u8>, i32)> {
    let g = gpu().map_err(|e| anyhow!("wgpu unavailable: {e}"))?;
    let eng = engine(g);
    let (staging, out_len, mips) = record_encode(
        g, eng, rgba, width, height, mip_count, flip, srgb, perceptual, profile,
    )?;
    blocking_readback(g, eng, staging, out_len, mips)
}

/// One texture's inputs for [`encode_bc7_mip_chain_batch`]; same fields as
/// the positional arguments of [`encode_bc7_mip_chain`].
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct MipChainJob<'a> {
    pub rgba: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub mip_count: Option<i32>,
    pub flip: bool,
    pub srgb: bool,
    pub perceptual: bool,
    pub profile: Bc7Profile,
}

/// Double-buffered pipeline over several textures: each texture's dispatch is
/// submitted before the previous texture's readback is awaited, so CPU-side
/// prep/submit for texture N overlaps GPU execution of texture N-1's tail
/// (two in-flight slots — this texture's just-submitted work, and the prior
/// texture's pending readback — is enough to close the gap; a third slot
/// would only help if per-texture CPU prep exceeded GPU time, which it does
/// not here). Results are returned in the same order as `jobs`, byte-for-byte
/// identical to calling [`encode_bc7_mip_chain`] once per job — the pooled
/// buffers reused across jobs are only handed back to the pool once actually
/// consumed (readback buffers after their map completes; scratch/working
/// buffers only after their own submit, exactly as the single-texture path
/// already does), and wgpu's single queue serializes access to any buffer a
/// later job's write_buffer/dispatch reuses from the pool.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn encode_bc7_mip_chain_batch(jobs: &[MipChainJob]) -> Result<Vec<(Vec<u8>, i32)>> {
    let g = gpu().map_err(|e| anyhow!("wgpu unavailable: {e}"))?;
    let eng = engine(g);
    let mut results = Vec::with_capacity(jobs.len());
    let mut pending: Option<(::wgpu::Buffer, u64, i32)> = None;
    for job in jobs {
        let submitted = record_encode(
            g,
            eng,
            job.rgba,
            job.width,
            job.height,
            job.mip_count,
            job.flip,
            job.srgb,
            job.perceptual,
            job.profile,
        )?;
        if let Some((staging, out_len, mips)) = pending.replace(submitted) {
            results.push(blocking_readback(g, eng, staging, out_len, mips)?);
        }
    }
    if let Some((staging, out_len, mips)) = pending {
        results.push(blocking_readback(g, eng, staging, out_len, mips)?);
    }
    Ok(results)
}

#[cfg(target_arch = "wasm32")]
#[allow(clippy::too_many_arguments)]
pub async fn encode_bc7_mip_chain_on(
    g: &Gpu,
    eng: &Engine,
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    srgb: bool,
    perceptual: bool,
    profile: Bc7Profile,
) -> Result<(Vec<u8>, i32)> {
    let (staging, out_len, mips) = record_encode(
        g, eng, rgba, width, height, mip_count, flip, srgb, perceptual, profile,
    )?;
    let slice = staging.slice(0..out_len);
    map_read(slice)
        .await
        .map_err(|e| anyhow!("wgpu readback map failed: {e:?}"))?;
    let out = slice
        .get_mapped_range()
        .map_err(|e| anyhow!("wgpu mapped range failed: {e:?}"))?
        .to_vec();
    staging.unmap();
    eng.put_readback(staging);
    Ok((out, mips))
}

#[cfg(target_arch = "wasm32")]
struct MapShared {
    result: Option<std::result::Result<(), ::wgpu::BufferAsyncError>>,
    waker: Option<std::task::Waker>,
}

#[cfg(target_arch = "wasm32")]
struct MapFuture(std::rc::Rc<std::cell::RefCell<MapShared>>);

#[cfg(target_arch = "wasm32")]
impl std::future::Future for MapFuture {
    type Output = std::result::Result<(), ::wgpu::BufferAsyncError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut st = self.0.borrow_mut();
        if let Some(r) = st.result.take() {
            std::task::Poll::Ready(r)
        } else {
            st.waker = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    }
}

#[cfg(target_arch = "wasm32")]
async fn map_read(
    slice: ::wgpu::BufferSlice<'_>,
) -> std::result::Result<(), ::wgpu::BufferAsyncError> {
    let shared = std::rc::Rc::new(std::cell::RefCell::new(MapShared {
        result: None,
        waker: None,
    }));
    let s2 = shared.clone();
    slice.map_async(::wgpu::MapMode::Read, move |r| {
        let mut st = s2.borrow_mut();
        st.result = Some(r);
        if let Some(w) = st.waker.take() {
            w.wake();
        }
    });
    MapFuture(shared).await
}

#[cfg(target_arch = "wasm32")]
pub mod bisect;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod testsup;
