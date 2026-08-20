use super::*;
use crate::gpu::corelib::bc7::probe;
use crate::gpu::corelib::bc7::{encode_group, group_signature, GROUP_WIDTH};
use crate::gpu::corelib::mips;
use crate::gpu::corelib::mode_tree::TREE;

struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg { state: seed }
    }

    fn next_byte(&mut self) -> u8 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.state >> 33) as u8
    }
}

fn gen_texture(seed: u64, w: u32, h: u32) -> Vec<u8> {
    let len = w as usize * h as usize * 4;
    let mut lcg = Lcg::new(seed);
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        out.push(lcg.next_byte());
    }
    out
}

const WG: u64 = 256;
const CONST_DUMP_WORDS: usize = 9490;
const PACK_CAP: usize = 20_000;

#[derive(Debug, Clone)]
pub struct FirstDiff {
    pub byte_offset: u32,
    pub got_word: u32,
    pub want_word: u32,
    pub case_index: u32,
}

#[derive(Debug, Clone)]
pub struct EntryResult {
    pub entry: String,
    pub cases: u32,
    pub pass: bool,
    pub first_diff: Option<FirstDiff>,
}

fn prepare_kernel_const(
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

#[allow(clippy::too_many_arguments)]
async fn dispatch_prepared_wg_pad(
    g: &Gpu,
    pipeline: &::wgpu::ComputePipeline,
    total: u32,
    n_items: u32,
    storages: &[(u32, &[u8])],
    readback: u32,
    wg: u64,
    pad: u32,
) -> std::result::Result<Vec<u8>, String> {
    use ::wgpu::util::DeviceExt;
    let mut meta = Vec::with_capacity(16);
    for v in [n_items, total, 0u32, pad] {
        meta.extend_from_slice(&v.to_le_bytes());
    }
    let meta_buf = g
        .device
        .create_buffer_init(&::wgpu::util::BufferInitDescriptor {
            label: Some("meta"),
            contents: &meta,
            usage: ::wgpu::BufferUsages::UNIFORM,
        });
    let bufs: Vec<(u32, ::wgpu::Buffer)> = storages
        .iter()
        .map(|(binding, data)| {
            (
                *binding,
                g.device
                    .create_buffer_init(&::wgpu::util::BufferInitDescriptor {
                        label: None,
                        contents: data,
                        usage: ::wgpu::BufferUsages::STORAGE | ::wgpu::BufferUsages::COPY_SRC,
                    }),
            )
        })
        .collect();
    let layout = pipeline.get_bind_group_layout(0);
    let mut entries = vec![::wgpu::BindGroupEntry {
        binding: 0,
        resource: meta_buf.as_entire_binding(),
    }];
    for (binding, buf) in &bufs {
        entries.push(::wgpu::BindGroupEntry {
            binding: *binding,
            resource: buf.as_entire_binding(),
        });
    }
    let bind_group = g.device.create_bind_group(&::wgpu::BindGroupDescriptor {
        label: None,
        layout: &layout,
        entries: &entries,
    });
    let (_, rb_buf) = bufs
        .iter()
        .find(|(b, _)| *b == readback)
        .ok_or("readback binding present")?;
    let rb_size = storages
        .iter()
        .find(|(b, _)| *b == readback)
        .ok_or("readback binding present")?
        .1
        .len() as u64;
    let staging = g.device.create_buffer(&::wgpu::BufferDescriptor {
        label: Some("staging"),
        size: rb_size,
        usage: ::wgpu::BufferUsages::MAP_READ | ::wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = g.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups((total as u64).div_ceil(wg) as u32, 1, 1);
    }
    enc.copy_buffer_to_buffer(rb_buf, 0, &staging, 0, rb_size);
    g.queue.submit([enc.finish()]);
    let slice = staging.slice(..);
    super::map_read(slice)
        .await
        .map_err(|e| format!("readback map failed: {e:?}"))?;
    let out = slice
        .get_mapped_range()
        .map_err(|e| format!("mapped range failed: {e:?}"))?
        .to_vec();
    staging.unmap();
    Ok(out)
}

struct Harness {
    module: ::wgpu::ShaderModule,
    blockify: ::wgpu::ShaderModule,
}

impl Harness {
    async fn run_kernel(
        &self,
        g: &Gpu,
        entry: &str,
        total: u32,
        n_items: u32,
        storages: &[(u32, &[u8])],
        readback: u32,
    ) -> std::result::Result<Vec<u8>, String> {
        let pipeline = prepare_kernel_const(g, &self.module, entry, &[]);
        let one = 1.0f32.to_bits();
        dispatch_prepared_wg_pad(g, &pipeline, total, n_items, storages, readback, WG, one).await
    }

    /// Common tail of every single-kernel entry: input at binding 4, an
    /// all-zero output of `cases * per_case` bytes at binding 3 (plus any
    /// `extra` storages), then compare against `want` at a `per_case` stride.
    #[allow(clippy::too_many_arguments)]
    async fn check_bytes(
        &self,
        g: &Gpu,
        entry: &str,
        name: String,
        extra: &[(u32, &[u8])],
        input_bytes: &[u8],
        want: &[u8],
        cases: usize,
        per_case: usize,
    ) -> EntryResult {
        let total = cases as u32;
        let out = vec![0u8; cases * per_case];
        let mut storages: Vec<(u32, &[u8])> = extra.to_vec();
        storages.push((4, input_bytes));
        storages.push((3, &out));
        match self.run_kernel(g, entry, total, 0, &storages, 3).await {
            Ok(got) => compare(name, total, per_case as u32, &got, want),
            Err(e) => error_result(&name, total, &e),
        }
    }

    /// `check_bytes` for word-shaped IO; `wpc` is output words per case.
    #[allow(clippy::too_many_arguments)]
    async fn check_as(
        &self,
        g: &Gpu,
        entry: &str,
        name: String,
        extra: &[(u32, &[u8])],
        input: &[u32],
        want: &[u32],
        cases: usize,
        wpc: usize,
    ) -> EntryResult {
        let (ib, wb) = (words_bytes(input), words_bytes(want));
        self.check_bytes(g, entry, name, extra, &ib, &wb, cases, wpc * 4)
            .await
    }

    async fn check(
        &self,
        g: &Gpu,
        entry: &str,
        input: &[u32],
        want: &[u32],
        cases: usize,
        wpc: usize,
    ) -> EntryResult {
        self.check_as(g, entry, entry.to_string(), &[], input, want, cases, wpc)
            .await
    }
}

fn compare(entry: String, cases: u32, stride_bytes: u32, got: &[u8], want: &[u8]) -> EntryResult {
    if got.len() != want.len() {
        return EntryResult {
            entry: format!(
                "{entry} [length mismatch got {} want {}]",
                got.len(),
                want.len()
            ),
            cases,
            pass: false,
            first_diff: Some(FirstDiff {
                byte_offset: got.len().min(want.len()) as u32,
                got_word: 0,
                want_word: 0,
                case_index: 0,
            }),
        };
    }
    match got.iter().zip(want.iter()).position(|(a, b)| a != b) {
        None => EntryResult {
            entry,
            cases,
            pass: true,
            first_diff: None,
        },
        Some(i) => {
            let w = i / 4 * 4;
            let word =
                |b: &[u8], w: usize| u32::from_le_bytes([b[w], b[w + 1], b[w + 2], b[w + 3]]);
            let stride = if stride_bytes > 0 { stride_bytes } else { 4 };
            let mut diff_words = 0u32;
            let mut diff_cases: Vec<u32> = Vec::new();
            let mut samples: Vec<String> = Vec::new();
            let mut k = 0usize;
            while k + 4 <= got.len() {
                if word(got, k) != word(want, k) {
                    diff_words += 1;
                    let case = (k as u32) / stride;
                    if diff_cases.last() != Some(&case) {
                        diff_cases.push(case);
                    }
                    if samples.len() < 6 {
                        samples.push(format!(
                            "case {case} word {} got=0x{:08x} want=0x{:08x}",
                            (k as u32 % stride) / 4,
                            word(got, k),
                            word(want, k)
                        ));
                    }
                }
                k += 4;
            }
            EntryResult {
                entry: format!(
                    "{entry} [diff_words={diff_words} diff_cases={} :: {}]",
                    diff_cases.len(),
                    samples.join(" | ")
                ),
                cases,
                pass: false,
                first_diff: Some(FirstDiff {
                    byte_offset: i as u32,
                    got_word: word(got, w),
                    want_word: word(want, w),
                    case_index: (i as u32) / stride,
                }),
            }
        }
    }
}

fn error_result(entry: &str, cases: u32, err: &str) -> EntryResult {
    EntryResult {
        entry: format!("{entry} SKIPPED: {err}"),
        cases,
        pass: false,
        first_diff: None,
    }
}

fn params4() -> [Params; 4] {
    [
        Params::slow(false),
        Params::slow(true),
        Params::basic(false),
        Params::basic(true),
    ]
}

fn block_with(f: impl Fn(usize) -> [u8; 4]) -> [u8; 64] {
    let mut b = [0u8; 64];
    for i in 0..16 {
        b[i * 4..i * 4 + 4].copy_from_slice(&f(i));
    }
    b
}

fn solid_block(px: [u8; 4]) -> [u8; 64] {
    block_with(|_| px)
}

fn classify_cases() -> (Vec<[u8; 64]>, Vec<u32>) {
    let blocks = vec![
        solid_block([10, 20, 30, 255]),
        solid_block([10, 20, 30, 100]),
        block_with(|i| [40, 50, 60, if i == 0 { 0 } else { 255 }]),
        block_with(|i| [40, 50, 60, if i % 2 == 0 { 254 } else { 255 }]),
        block_with(|i| [i as u8 * 3, 200 - i as u8, i as u8, 255]),
        block_with(|i| [77, 88, if i == 15 { 100 } else { 99 }, 255]),
    ];
    let want = vec![0u32, 0, 1, 1, 2, 2];
    (blocks, want)
}

fn xs64(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn weight_sets() -> Vec<[u32; 4]> {
    let mut sets: Vec<[u32; 4]> = Vec::new();
    for p in params4() {
        let mut muled = p.weights;
        for c in 0..4 {
            muled[c] *= p.mode67_weight_mul[c];
        }
        for w in [p.weights, muled] {
            if !sets.contains(&w) {
                sets.push(w);
            }
        }
    }
    sets.push([37, 5, 11, 3]);
    sets
}

fn corner_colors() -> Vec<[i32; 4]> {
    (0..16u32)
        .map(|m| {
            let mut c = [0i32; 4];
            for k in 0..4 {
                if (m >> k) & 1 == 1 {
                    c[k] = 255;
                }
            }
            c
        })
        .collect()
}

fn gen_block(st: &mut u64, strategy: usize, out: &mut [u8]) {
    let byte = |st: &mut u64| (xs64(st) % 256) as u8;
    match strategy % 9 {
        0 => {
            for b in out.iter_mut() {
                *b = byte(st);
            }
        }
        2 => {
            let a = [byte(st), byte(st), byte(st), 255];
            let b = [byte(st), byte(st), byte(st), 255];
            for i in 0..16 {
                let px = if i % 2 == 0 { a } else { b };
                out[i * 4..i * 4 + 4].copy_from_slice(&px);
            }
        }
        3 => {
            let base = [
                (xs64(st) % 250) as u8,
                (xs64(st) % 250) as u8,
                (xs64(st) % 250) as u8,
                255,
            ];
            for i in 0..16 {
                for k in 0..3 {
                    out[i * 4 + k] = base[k] + (xs64(st) % 5) as u8;
                }
                out[i * 4 + 3] = 255;
            }
        }
        4 => {
            for i in 0..16u8 {
                let o = i as usize * 4;
                out[o] = i * 16;
                out[o + 1] = 255 - i * 8;
                out[o + 2] = i * 4;
                out[o + 3] = i * 17;
            }
        }
        5 => {
            for i in 0..16u8 {
                let o = i as usize * 4;
                out[o] = i * 15;
                out[o + 1] = 240 - i * 12;
                out[o + 2] = 30 + i * 9;
                out[o + 3] = 255;
            }
        }
        6 => {
            for i in 0..16 {
                for k in 0..3 {
                    out[i * 4 + k] = byte(st);
                }
                out[i * 4 + 3] = 255;
            }
        }
        s => {
            let px = [
                byte(st),
                byte(st),
                byte(st),
                if s == 1 { byte(st) } else { 255 },
            ];
            for i in 0..16 {
                out[i * 4..i * 4 + 4].copy_from_slice(&px);
            }
            if s == 8 {
                let j = (xs64(st) % 16) as usize;
                out[j * 4] = out[j * 4].wrapping_add(40);
                out[j * 4 + 3] = 200;
            }
        }
    }
}

fn push_pixels(input: &mut Vec<u32>, px: &[[i32; 4]; 16]) {
    for row in px {
        for &q in row {
            input.push(q as u32);
        }
    }
}

fn px_from_block(blk: &[u8; 64]) -> [[i32; 4]; 16] {
    let mut px = [[0i32; 4]; 16];
    for i in 0..16 {
        for k in 0..4 {
            px[i][k] = blk[i * 4 + k] as i32;
        }
    }
    px
}

fn push_i32s(v: &mut Vec<u32>, s: &[i32]) {
    for &x in s {
        v.push(x as u32);
    }
}

fn push_f32s(v: &mut Vec<u32>, s: &[f32]) {
    for &f in s {
        v.push(f.to_bits());
    }
}

fn push_u64(v: &mut Vec<u32>, x: u64) {
    v.push(x as u32);
    v.push((x >> 32) as u32);
}

fn expected_const_words() -> Vec<u32> {
    let mut v = Vec::with_capacity(CONST_DUMP_WORDS);
    v.extend_from_slice(probe::weights2());
    v.extend_from_slice(probe::weights3());
    v.extend_from_slice(probe::weights4());
    for row in probe::weights2x() {
        push_f32s(&mut v, row);
    }
    for row in probe::weights3x() {
        push_f32s(&mut v, row);
    }
    for row in probe::weights4x() {
        push_f32s(&mut v, row);
    }
    for &b in probe::partition2().iter() {
        v.push(b as u32);
    }
    for &b in probe::partition3().iter() {
        v.push(b as u32);
    }
    push_i32s(&mut v, probe::anchor_2nd());
    push_i32s(&mut v, probe::anchor_3rd_1());
    push_i32s(&mut v, probe::anchor_3rd_2());
    for &n in probe::num_subsets().iter() {
        v.push(n as u32);
    }
    v.extend_from_slice(probe::partition_bits());
    v.extend_from_slice(probe::color_index_bitcount());
    push_i32s(&mut v, probe::alpha_index_bitcount());
    push_i32s(&mut v, probe::mode_has_p_bits());
    push_i32s(&mut v, probe::mode_has_shared_p_bits());
    v.extend_from_slice(probe::color_precision_table());
    v.extend_from_slice(probe::alpha_precision_table());
    v.push(probe::pr_weight().to_bits());
    v.push(probe::pb_weight().to_bits());
    v.extend_from_slice(&probe::mode_idx_words());
    v.push(probe::checkerboard_partition_index());
    for get in [
        &probe::subset_idx2 as &dyn Fn(usize) -> ([[i32; 16]; 3], [u32; 3]),
        &probe::subset_idx3,
    ] {
        for p in 0..64 {
            let (idx, _) = get(p);
            for s in 0..3 {
                push_i32s(&mut v, &idx[s]);
            }
        }
        for p in 0..64 {
            let (_, tot) = get(p);
            v.extend_from_slice(&tot);
        }
    }
    for n in TREE.iter() {
        v.push(n.feature as i32 as u32);
        v.push(n.threshold as u32);
        v.push(n.left as i32 as u32);
        v.push(n.right as i32 as u32);
    }
    assert_eq!(v.len(), CONST_DUMP_WORDS);
    v
}

impl Harness {
    async fn entry_tables(&self, g: &Gpu) -> Vec<EntryResult> {
        let t = build_opt_tables();
        let opt = opt_tables_words(&t);
        let opt_bytes = words_bytes(&opt);
        let total_words = CONST_DUMP_WORDS + opt.len();
        let total = total_words as u32;
        let out = vec![0u8; total_words * 4];
        let mut want_full = expected_const_words();
        want_full.extend_from_slice(&opt);
        let small: &[(usize, usize)] = &[
            (0, 140),
            (2188, 2454),
            (8982, CONST_DUMP_WORDS),
            (CONST_DUMP_WORDS, total_words),
        ];
        let priv1: &[(usize, usize)] = &[(140, 2188), (5526, 5718), (8790, 8982)];
        let priv2: &[(usize, usize)] = &[(2454, 5526), (5718, 8790)];
        let masked = |ranges: &[(usize, usize)]| -> Vec<u32> {
            let mut w = vec![0u32; total_words];
            for &(a, b) in ranges {
                w[a..b].copy_from_slice(&want_full[a..b]);
            }
            w
        };
        let mut results = Vec::new();
        let plans: [(&str, &[(usize, usize)], Vec<(u32, &[u8])>); 3] = [
            ("bc7_test_tables", small, vec![(2, &opt_bytes), (3, &out)]),
            ("bc7_test_tables_priv1", priv1, vec![(3, &out)]),
            ("bc7_test_tables_priv2", priv2, vec![(3, &out)]),
        ];
        for (entry, ranges, storages) in plans {
            let want = words_bytes(&masked(ranges));
            match self.run_kernel(g, entry, total, 0, &storages, 3).await {
                Ok(got) => results.push(compare(entry.to_string(), total, 4, &got, &want)),
                Err(e) => results.push(error_result(entry, total, &e)),
            }
        }
        results
    }
}

impl Harness {
    async fn entry_u64ops(&self, g: &Gpu) -> EntryResult {
        let vals: Vec<u64> = vec![
            0,
            1,
            2,
            3,
            0x7fff,
            0x8000,
            0xffff,
            0x10000,
            0x7fffffff,
            0x80000000,
            0xfffffffe,
            0xffffffff,
            0x100000000,
            0x1ffffffff,
            8191,
            8192,
            8193,
            0x123456789abcdef0,
            0x8000000000000000,
            0xfffffffffffffffe,
            u64::MAX,
            (1u64 << 45) + 12345,
            1u64 << 33,
            1u64 << 31,
        ];
        let mut pairs: Vec<(u64, u64)> = Vec::new();
        for &a in &vals {
            for &b in &vals {
                pairs.push((a, b));
            }
        }
        let mut st = 0x9e3779b97f4a7c15u64;
        for _ in 0..400 {
            pairs.push((xs64(&mut st), xs64(&mut st)));
        }
        let fs: Vec<f32> = vec![
            0.0,
            0.25,
            0.5,
            0.75,
            0.999,
            1.0,
            1.5,
            2.0,
            255.0,
            65535.75,
            8323200.0,
            30000000.5,
            123456789.0,
            2147483520.0,
            33554431.5,
        ];
        let mut input: Vec<u32> = Vec::new();
        let mut want: Vec<u32> = Vec::new();
        for (i, &(a, b)) in pairs.iter().enumerate() {
            let f = fs[i % fs.len()];
            let sh = 1 + (i as u32) % 31;
            for v in [a, b] {
                push_u64(&mut input, v);
            }
            input.push(f.to_bits());
            input.push(sh);
            push_u64(&mut want, a.wrapping_add(b));
            push_u64(&mut want, a.saturating_add(b));
            push_u64(&mut want, a.saturating_mul(b));
            push_u64(&mut want, (a as u32 as u64) * (b as u32 as u64));
            want.push((a < b) as u32 | (((a <= b) as u32) << 1) | (((a == b) as u32) << 2));
            push_u64(&mut want, a >> sh);
            let conv = f as i64 as u64;
            assert!(conv < 1u64 << 31, "conv case {f} out of proven band");
            want.push(conv as u32);
        }
        self.check(g, "bc7_test_u64ops", &input, &want, pairs.len(), 12)
            .await
    }

    async fn entry_vecmath(&self, g: &Gpu) -> EntryResult {
        let mut cases: Vec<([f32; 4], [f32; 4], [i32; 4], u32, bool, f32, i32)> = vec![
            ([0.0; 4], [0.0; 4], [0, 0, 0, 0], 4, false, 0.0, 0),
            (
                [1.0, 0.0, 0.0, 0.0],
                [1.0, 2.0, 3.0, 4.0],
                [15, 7, 3, 1],
                7,
                true,
                1.0,
                -5,
            ),
            (
                [3.0, -4.0, 12.0, 0.5],
                [-1.5, 2.5, 0.25, 8.0],
                [31, 0, 31, 16],
                5,
                false,
                -0.5,
                i32::MIN + 1,
            ),
            (
                [255.0, 255.0, 255.0, 255.0],
                [255.0; 4],
                [63, 63, 63, 63],
                6,
                true,
                1.5,
                i32::MAX,
            ),
        ];
        for f in [
            3.0f32,
            -3.0,
            0.1,
            7.0,
            10.0,
            1.0e-30,
            1.0e30,
            6.931_472,
            2.0,
            4.0,
            1.0 / 3.0,
            0.999_999_94,
            1.000_000_1,
        ] {
            cases.push((
                [1.0, 2.0, 3.0, 4.0],
                [4.0, 3.0, 2.0, 1.0],
                [5, 9, 2, 7],
                5,
                true,
                f,
                42,
            ));
        }
        let mut st = 0x5851f42d4c957f2du64;
        for _ in 0..500 {
            let rf = |st: &mut u64| ((xs64(st) % 400_000) as f32 - 200_000.0) / 128.0;
            let v = [rf(&mut st), rf(&mut st), rf(&mut st), rf(&mut st)];
            let a = [rf(&mut st), rf(&mut st), rf(&mut st), rf(&mut st)];
            let comp_bits = 4 + (xs64(&mut st) % 4) as u32;
            let has_pbits = xs64(&mut st) & 1 == 1;
            let nbits = comp_bits + has_pbits as u32;
            let mut c = [0i32; 4];
            for k in 0..4 {
                c[k] = (xs64(&mut st) & ((1u64 << nbits) - 1)) as i32;
            }
            let f = rf(&mut st);
            let mut x = xs64(&mut st) as i32;
            if x == i32::MIN {
                x = 0;
            }
            cases.push((v, a, c, comp_bits, has_pbits, f, x));
        }
        let mut input: Vec<u32> = Vec::new();
        let mut want: Vec<u32> = Vec::new();
        for &(v, a, c, comp_bits, has_pbits, f, x) in &cases {
            push_f32s(&mut input, &v);
            push_f32s(&mut input, &a);
            push_i32s(&mut input, &c);
            input.push(comp_bits);
            input.push(has_pbits as u32);
            input.push(f.to_bits());
            input.push(x as u32);
            push_f32s(&mut want, &probe::vec4f_normalize(v));
            want.push(probe::vec4f_dot(v, a).to_bits());
            push_i32s(&mut want, &probe::scale_color(c, comp_bits, has_pbits));
            want.push(probe::saturate(f).to_bits());
            want.push(probe::itrunc(f) as u32);
            want.push(probe::iabs32(x) as u32);
            want.push(probe::sq(f).to_bits());
            let i = (has_pbits as usize) % 4;
            want.push(match comp_bits % 3 {
                0 => probe::weights2()[i],
                1 => probe::weights3()[i],
                _ => probe::weights4()[i],
            });
            if f != 0.0 {
                want.push((1.0f32 / f).to_bits());
                want.push(f.abs().sqrt().to_bits());
            } else {
                want.push(0);
                want.push(0);
            }
        }
        self.check(g, "bc7_test_vecmath", &input, &want, cases.len(), 16)
            .await
    }
}

fn dist_cases() -> Vec<([i32; 4], [i32; 4], [u32; 4], bool)> {
    let sets = weight_sets();
    let corners = corner_colors();
    let mut cases = Vec::new();
    for &w in &sets {
        for perc in [false, true] {
            for e1 in &corners {
                for e2 in &corners {
                    cases.push((*e1, *e2, w, perc));
                }
            }
        }
    }
    let mut st = 0x2545f4914f6cdd1du64;
    for _ in 0..2000 {
        let mut e1 = [0i32; 4];
        let mut e2 = [0i32; 4];
        for k in 0..4 {
            e1[k] = (xs64(&mut st) % 256) as i32;
            e2[k] = (xs64(&mut st) % 256) as i32;
        }
        let w = sets[(xs64(&mut st) % sets.len() as u64) as usize];
        let perc = xs64(&mut st) & 1 == 1;
        cases.push((e1, e2, w, perc));
    }
    cases
}

impl Harness {
    async fn entry_dist(
        &self,
        g: &Gpu,
        entry: &str,
        host: impl Fn([i32; 4], [i32; 4], bool, [u32; 4]) -> u64,
    ) -> EntryResult {
        let cases = dist_cases();
        let mut input: Vec<u32> = Vec::new();
        let mut want: Vec<u32> = Vec::new();
        for &(e1, e2, w, perc) in &cases {
            push_i32s(&mut input, &e1);
            push_i32s(&mut input, &e2);
            input.extend_from_slice(&w);
            input.push(perc as u32);
            input.extend_from_slice(&[0, 0, 0]);
            let d = host(e1, e2, perc, w);
            assert!(d < 1u64 << 31, "{entry}: host dist out of proven band");
            push_u64(&mut want, d);
        }
        self.check(g, entry, &input, &want, cases.len(), 2).await
    }
}

fn lsq_cases() -> Vec<(u32, u32, [i32; 16], [[i32; 4]; 16])> {
    let mut st = 0x94d049bb133111ebu64;
    let mut cases = Vec::new();
    for tbl in 0..3u32 {
        let tlen = probe::weightsx_table_len(tbl as usize) as u64;
        for ci in 0..140usize {
            let n = 1 + (xs64(&mut st) % 16) as u32;
            let mut sel = [0i32; 16];
            let mut colors = [[0i32; 4]; 16];
            for i in 0..16 {
                sel[i] = (xs64(&mut st) % tlen) as i32;
                for k in 0..4 {
                    colors[i][k] = (xs64(&mut st) % 256) as i32;
                }
            }
            if ci % 7 == 0 {
                sel = [(xs64(&mut st) % tlen) as i32; 16];
            }
            if ci % 11 == 0 {
                colors = [colors[0]; 16];
            }
            if ci % 13 == 0 {
                for (i, s) in sel.iter_mut().enumerate() {
                    *s = if i % 2 == 0 { 0 } else { (tlen - 1) as i32 };
                }
            }
            cases.push((n, tbl, sel, colors));
        }
    }
    cases
}

fn lsq_input_words(cases: &[(u32, u32, [i32; 16], [[i32; 4]; 16])]) -> Vec<u32> {
    let mut input = Vec::with_capacity(cases.len() * 84);
    for &(n, tbl, sel, colors) in cases {
        input.push(n);
        input.push(tbl);
        input.extend_from_slice(&[0, 0]);
        for s in sel {
            input.push(s as u32);
        }
        for c in colors {
            for q in c {
                input.push(q as u32);
            }
        }
    }
    input
}

impl Harness {
    async fn entry_lsq(&self, g: &Gpu, entry: &str, width: usize) -> EntryResult {
        let cases = lsq_cases();
        let input = lsq_input_words(&cases);
        let mut want: Vec<u32> = Vec::new();
        for &(n, tbl, sel, colors) in &cases {
            match entry {
                "bc7_test_lsq_rgba" => {
                    let (xl, xh) = probe::lsq_rgba(n as usize, &sel, tbl as usize, &colors);
                    push_f32s(&mut want, &xl);
                    push_f32s(&mut want, &xh);
                }
                "bc7_test_lsq_rgb" => {
                    let (xl, xh) = probe::lsq_rgb(n as usize, &sel, tbl as usize, &colors);
                    push_f32s(&mut want, &xl);
                    push_f32s(&mut want, &xh);
                }
                _ => {
                    let (xl, xh) = probe::lsq_a(n as usize, &sel, tbl as usize, &colors);
                    want.push(xl.to_bits());
                    want.push(xh.to_bits());
                }
            }
        }
        let total = cases.len() as u32;
        self.check(g, entry, &input, &want, cases.len(), width)
            .await
    }
}

fn pack_colors() -> Vec<[i32; 4]> {
    let mut v = corner_colors();
    for x in 0..256i32 {
        v.push([x, x, x, 255]);
        v.push([x, x, x, x]);
        v.push([x, 255 - x, x / 2, 255]);
        v.push([255 - x, 0, x, 128]);
    }
    let mut st = 0x1234_5678_9abc_def0u64;
    while v.len() < 4300 {
        let mut c = [0i32; 4];
        for k in 0..4 {
            c[k] = (xs64(&mut st) % 256) as i32;
        }
        v.push(c);
    }
    v
}

struct PackCase {
    color: [i32; 4],
    nsw: u32,
    perceptual: bool,
    weights: [u32; 4],
    num_pixels: usize,
    pixels: [[i32; 4]; 16],
}

fn pack_cases(nsw_options: &[u32]) -> Vec<PackCase> {
    let colors = pack_colors();
    let sets = weight_sets();
    let mut st = 0x0123_4567_89ab_cdefu64;
    let mut cases = Vec::new();
    for &color in &colors {
        for &weights in &sets {
            for perceptual in [false, true] {
                for &nsw in nsw_options {
                    let num_pixels = 1 + (xs64(&mut st) % 16) as usize;
                    let mut pixels = [color; 16];
                    if xs64(&mut st).is_multiple_of(16) {
                        for px in pixels.iter_mut() {
                            for k in 0..4 {
                                px[k] = (xs64(&mut st) % 256) as i32;
                            }
                        }
                    }
                    cases.push(PackCase {
                        color,
                        nsw,
                        perceptual,
                        weights,
                        num_pixels,
                        pixels,
                    });
                }
            }
        }
    }
    cases
}

impl Harness {
    async fn entry_pack(
        &self,
        g: &Gpu,
        entry: &str,
        nsw_options: &[u32],
        host: impl Fn(u32, bool, [u32; 4], [usize; 4], usize, &[[i32; 4]; 16]) -> probe::PackOut,
    ) -> EntryResult {
        let t = build_opt_tables();
        let opt_bytes = words_bytes(&opt_tables_words(&t));
        let mut cases = pack_cases(nsw_options);
        let full = cases.len();
        let name = if cases.len() > PACK_CAP {
            cases.truncate(PACK_CAP);
            format!("{entry}[capped:{PACK_CAP}/{full}]")
        } else {
            entry.to_string()
        };
        let mut input: Vec<u32> = Vec::with_capacity(cases.len() * 75);
        let mut want: Vec<u32> = Vec::with_capacity(cases.len() * 28);
        for c in &cases {
            input.push(c.num_pixels as u32);
            push_i32s(&mut input, &c.color);
            input.push(c.nsw);
            input.push(c.perceptual as u32);
            input.extend_from_slice(&c.weights);
            push_pixels(&mut input, &c.pixels);
            let rgba = c.color.map(|q| q as usize);
            let (low, high, pbits, sel, err) = host(
                c.nsw,
                c.perceptual,
                c.weights,
                rgba,
                c.num_pixels,
                &c.pixels,
            );
            push_i32s(&mut want, &low);
            push_i32s(&mut want, &high);
            want.extend_from_slice(&pbits);
            push_i32s(&mut want, &sel);
            push_u64(&mut want, err);
        }
        let x = [(2, &opt_bytes[..])];
        self.check_as(g, entry, name, &x, &input, &want, cases.len(), 28)
            .await
    }

    async fn entry_fixdeg(&self, g: &Gpu) -> EntryResult {
        let mut st = 0xfeed_face_dead_beefu64;
        let mut cases: Vec<(u32, i32, [i32; 4], [i32; 4], [f32; 4], [f32; 4])> = Vec::new();
        let iscales = [3i32, 7, 15, 31, 63, 127];
        for mode in [0u32, 1, 2, 4, 6, 7] {
            for &iscale in &iscales {
                for ci in 0..120usize {
                    let mut tmin = [0i32; 4];
                    let mut tmax = [0i32; 4];
                    for k in 0..4 {
                        tmin[k] = (xs64(&mut st) % (iscale as u64 + 1)) as i32;
                        tmax[k] = (xs64(&mut st) % (iscale as u64 + 1)) as i32;
                    }
                    match ci % 5 {
                        0 => {
                            tmax = tmin;
                        }
                        1 => {
                            for k in 0..3 {
                                if xs64(&mut st) & 1 == 1 {
                                    tmax[k] = tmin[k];
                                }
                            }
                        }
                        2 => {
                            tmin = [0, iscale, iscale >> 1, 0];
                            tmax = tmin;
                        }
                        3 => {
                            tmin = [iscale, 0, (iscale >> 1) + 1, iscale];
                            tmax = tmin;
                        }
                        _ => {}
                    }
                    let rf = |st: &mut u64| (xs64(st) % 2560) as f32 / 2559.0;
                    let mut xl = [rf(&mut st), rf(&mut st), rf(&mut st), rf(&mut st)];
                    let xh = [rf(&mut st), rf(&mut st), rf(&mut st), rf(&mut st)];
                    if ci % 3 == 0 {
                        xl = xh;
                    }
                    if ci % 7 == 0 {
                        xl[1] = xh[1];
                    }
                    cases.push((mode, iscale, tmin, tmax, xl, xh));
                }
            }
        }
        let mut input: Vec<u32> = Vec::new();
        let mut want: Vec<u32> = Vec::new();
        for &(mode, iscale, tmin, tmax, xl, xh) in &cases {
            input.push(mode);
            input.push(iscale as u32);
            push_i32s(&mut input, &tmin);
            push_i32s(&mut input, &tmax);
            push_f32s(&mut input, &xl);
            push_f32s(&mut input, &xh);
            let (a, b) = probe::fix_degenerate(mode as usize, tmin, tmax, xl, xh, iscale);
            push_i32s(&mut want, &a);
            push_i32s(&mut want, &b);
        }
        self.check(g, "bc7_test_fixdeg", &input, &want, cases.len(), 8)
            .await
    }
}

fn eval_buckets() -> Vec<(u32, u32, u32, bool, bool, bool)> {
    vec![
        (8, 1, 4, false, true, false),
        (8, 1, 6, false, true, true),
        (4, 0, 5, false, false, false),
        (4, 0, 7, false, true, false),
        (8, 1, 5, false, false, false),
        (4, 0, 7, false, false, false),
        (16, 2, 7, false, true, false),
        (16, 2, 7, true, true, false),
        (4, 0, 5, true, true, false),
        (16, 2, 7, true, false, false),
    ]
}

fn eval_cases() -> Vec<(probe::EvalCase, [[i32; 4]; 16])> {
    let sets = weight_sets();
    let mut st = 0x00c0_ffee_1234_5678u64;
    let mut cases = Vec::new();
    for (bi, &(nsw, tbl, comp_bits, has_alpha, has_pbits, share)) in
        eval_buckets().iter().enumerate()
    {
        for perceptual in [false, true] {
            for (wi, &weights) in sets.iter().enumerate() {
                for ci in 0..40usize {
                    let lim = 1u64 << comp_bits;
                    let mut low = [0i32; 4];
                    let mut high = [0i32; 4];
                    for k in 0..4 {
                        low[k] = (xs64(&mut st) % lim) as i32;
                        high[k] = (xs64(&mut st) % lim) as i32;
                    }
                    let mut pbits = [(xs64(&mut st) & 1) as u32, (xs64(&mut st) & 1) as u32];
                    match ci % 8 {
                        0 => {
                            high = low;
                        }
                        1 => {
                            high = low;
                            pbits = [pbits[0], pbits[0]];
                        }
                        2 => {
                            std::mem::swap(&mut low, &mut high);
                        }
                        3 => {
                            low = [0; 4];
                            high = [(lim - 1) as i32; 4];
                        }
                        4 => {
                            low = [(lim - 1) as i32; 4];
                            high = [0; 4];
                        }
                        _ => {}
                    }
                    let mut blk = [0u8; 64];
                    gen_block(&mut st, bi * 7 + wi * 3 + ci, &mut blk);
                    let mut pixels = [[0i32; 4]; 16];
                    for i in 0..16 {
                        for k in 0..4 {
                            pixels[i][k] = blk[i * 4 + k] as i32;
                        }
                    }
                    let num_pixels = if ci % 5 == 0 {
                        1 + (xs64(&mut st) % 16) as usize
                    } else {
                        16
                    };
                    let init_err = match ci % 9 {
                        0 => 0,
                        1 => 42,
                        _ => u64::MAX,
                    };
                    cases.push((
                        probe::EvalCase {
                            low,
                            high,
                            pbits,
                            nsw,
                            tbl: tbl as usize,
                            comp_bits,
                            weights,
                            has_alpha,
                            has_pbits,
                            share_pbit: share,
                            perceptual,
                            init_err,
                            num_pixels,
                        },
                        pixels,
                    ));
                }
            }
        }
    }
    cases
}

impl Harness {
    async fn entry_evalsol(&self, g: &Gpu) -> EntryResult {
        let cases = eval_cases();
        let mut input: Vec<u32> = Vec::with_capacity(cases.len() * 88);
        let mut want: Vec<u32> = Vec::with_capacity(cases.len() * 46);
        for (c, pixels) in &cases {
            input.push(c.nsw);
            input.push(c.tbl as u32);
            input.push(c.comp_bits);
            input.push(c.has_alpha as u32);
            input.push(c.has_pbits as u32);
            input.push(c.share_pbit as u32);
            input.push(c.perceptual as u32);
            input.push(c.num_pixels as u32);
            input.push(c.init_err as u32);
            input.push((c.init_err >> 32) as u32);
            input.extend_from_slice(&c.weights);
            push_i32s(&mut input, &c.low);
            push_i32s(&mut input, &c.high);
            input.extend_from_slice(&c.pbits);
            push_pixels(&mut input, pixels);
            let (ret, best, low, high, pbits, sel, seltmp) = probe::eval_solution(c, pixels);
            assert!(ret < 1u64 << 31, "evalsol: host total out of proven band");
            push_u64(&mut want, ret);
            push_u64(&mut want, best);
            push_i32s(&mut want, &low);
            push_i32s(&mut want, &high);
            want.extend_from_slice(&pbits);
            push_i32s(&mut want, &sel);
            push_i32s(&mut want, &seltmp);
        }
        self.check(g, "bc7_test_evalsol", &input, &want, cases.len(), 46)
            .await
    }
}

impl Harness {
    async fn entry_div(&self, g: &Gpu) -> EntryResult {
        let mut cases: Vec<(f32, f32)> = Vec::new();
        for a in -320i32..=320 {
            for b in 1i32..=40 {
                cases.push((a as f32, b as f32));
            }
        }
        for x in 0..=255i32 {
            cases.push((x as f32, 255.0));
            cases.push((-x as f32, 255.0));
        }
        cases.push((0.0, 3.0));
        cases.push((-0.0, 3.0));
        cases.push((0.0, -3.0));
        let mut st = 0xd1a1_50f2_55aa_1111u64;
        for _ in 0..24_000 {
            let gen = |st: &mut u64| -> f32 {
                let sign = (xs64(st) & 1) << 31;
                let exp = 117 + (xs64(st) % 31);
                let mant = xs64(st) & 0x7fffff;
                f32::from_bits(sign as u32 | ((exp as u32) << 23) | mant as u32)
            };
            cases.push((gen(&mut st), gen(&mut st)));
        }
        let mut input: Vec<u32> = Vec::with_capacity(cases.len() * 2);
        let mut want: Vec<u32> = Vec::with_capacity(cases.len());
        for &(a, b) in &cases {
            input.push(a.to_bits());
            input.push(b.to_bits());
            want.push((a / b).to_bits());
        }
        self.check(g, "bc7_test_div", &input, &want, cases.len(), 1)
            .await
    }
}

fn push_ccp(input: &mut Vec<u32>, c: &probe::CCP) {
    input.push(c.nsw);
    input.push(c.tbl as u32);
    input.push(c.comp_bits);
    input.push(c.has_alpha as u32);
    input.push(c.has_pbits as u32);
    input.push(c.share_pbit as u32);
    input.push(c.perceptual as u32);
    input.extend_from_slice(&c.weights);
}

fn push_init(input: &mut Vec<u32>, init: &probe::CCInit) {
    push_u64(input, init.err);
    push_i32s(input, &init.low);
    push_i32s(input, &init.high);
    input.extend_from_slice(&init.pbits);
}

fn push_ccout(want: &mut Vec<u32>, out: &probe::CCOut, ctx: &str) {
    let (err, low, high, pbits, sel, seltmp) = out;
    assert!(
        *err < 1u64 << 31,
        "{ctx}: host err {err} out of proven band"
    );
    push_u64(want, *err);
    push_i32s(want, low);
    push_i32s(want, high);
    want.extend_from_slice(pbits);
    push_i32s(want, sel);
    push_i32s(want, seltmp);
}

fn init_none() -> probe::CCInit {
    probe::CCInit {
        err: u64::MAX,
        low: [0; 4],
        high: [0; 4],
        pbits: [0, 0],
    }
}

fn rand_ep(st: &mut u64, lim: u64) -> [i32; 4] {
    let mut e = [0i32; 4];
    for k in 0..4 {
        e[k] = (xs64(st) % lim) as i32;
    }
    e
}

fn e4w_errs(
    c: &probe::CCP,
    lo: &[[i32; 4]; 2],
    hi: &[[i32; 4]; 2],
    n: usize,
    px: &[[i32; 4]; 16],
) -> [u64; 4] {
    let mut out = [0u64; 4];
    for k in 0..4usize {
        let ec = probe::EvalCase {
            low: lo[k >> 1],
            high: hi[k & 1],
            pbits: [(k >> 1) as u32, (k & 1) as u32],
            nsw: c.nsw,
            tbl: c.tbl,
            comp_bits: c.comp_bits,
            weights: c.weights,
            has_alpha: c.has_alpha,
            has_pbits: c.has_pbits,
            share_pbit: c.share_pbit,
            perceptual: c.perceptual,
            init_err: u64::MAX,
            num_pixels: n,
        };
        out[k] = probe::eval_solution(&ec, px).0;
    }
    out
}

struct E4Case {
    c: probe::CCP,
    lo: [[i32; 4]; 2],
    hi: [[i32; 4]; 2],
    init: probe::CCInit,
    n: usize,
    px: [[i32; 4]; 16],
}

impl Harness {
    async fn entry_eval4way(&self, g: &Gpu) -> EntryResult {
        let sets = weight_sets();
        let buckets: Vec<(u32, usize, u32, bool)> = vec![
            (16, 2, 7, true),
            (16, 2, 7, false),
            (8, 1, 6, false),
            (4, 0, 5, true),
        ];
        let mut st = 0xabad_1dea_0bad_c0deu64;
        let mut cases: Vec<E4Case> = Vec::new();
        for (bi, &(nsw, tbl, comp_bits, has_alpha)) in buckets.iter().enumerate() {
            for perceptual in [false, true] {
                for (wi, &weights) in sets.iter().enumerate() {
                    for ci in 0..60usize {
                        let c = probe::CCP {
                            nsw,
                            tbl,
                            comp_bits,
                            weights,
                            has_alpha,
                            has_pbits: true,
                            share_pbit: false,
                            perceptual,
                        };
                        let lim = 1u64 << comp_bits;
                        let mut lo = [rand_ep(&mut st, lim), rand_ep(&mut st, lim)];
                        let mut hi = [rand_ep(&mut st, lim), rand_ep(&mut st, lim)];
                        match ci % 6 {
                            0 => {
                                lo[1] = lo[0];
                                hi[1] = hi[0];
                            }
                            1 => {
                                hi[0] = lo[0];
                                hi[1] = lo[1];
                            }
                            2 => {
                                lo = [[0; 4]; 2];
                                hi = [[(lim - 1) as i32; 4]; 2];
                            }
                            _ => {}
                        }
                        let mut blk = [0u8; 64];
                        gen_block(&mut st, bi * 5 + wi * 3 + ci, &mut blk);
                        let px = px_from_block(&blk);
                        let n = if ci % 4 == 0 {
                            1 + (xs64(&mut st) % 16) as usize
                        } else {
                            16
                        };
                        let err = match ci % 9 {
                            0 => 0u64,
                            1 => 42,
                            _ => u64::MAX,
                        };
                        let init = probe::CCInit {
                            err,
                            low: rand_ep(&mut st, lim),
                            high: rand_ep(&mut st, lim),
                            pbits: [0, 0],
                        };
                        cases.push(E4Case {
                            c,
                            lo,
                            hi,
                            init,
                            n,
                            px,
                        });
                    }
                }
            }
        }
        let mine_c = probe::CCP {
            nsw: 16,
            tbl: 2,
            comp_bits: 7,
            weights: [1, 1, 1, 1],
            has_alpha: true,
            has_pbits: true,
            share_pbit: false,
            perceptual: false,
        };
        let mut ties_found = 0usize;
        for attempt in 0..60_000usize {
            if ties_found >= 120 {
                break;
            }
            let lim = 128u64;
            let base = rand_ep(&mut st, lim);
            let mut lo = [base, base];
            let mut hi = [rand_ep(&mut st, lim); 2];
            if attempt % 3 == 0 {
                lo = [rand_ep(&mut st, lim), rand_ep(&mut st, lim)];
                hi = [rand_ep(&mut st, lim), rand_ep(&mut st, lim)];
            }
            let mut blk = [0u8; 64];
            gen_block(
                &mut st,
                if attempt % 2 == 0 { 3 } else { attempt },
                &mut blk,
            );
            let px = px_from_block(&blk);
            let errs = e4w_errs(&mine_c, &lo, &hi, 16, &px);
            let mn = *errs.iter().min().unwrap();
            let band = mn.saturating_add(mn.saturating_mul(1) / 8192);
            let in_band = errs.iter().filter(|&&e| e <= band).count();
            let last_in = (0..4).filter(|&k| errs[k] <= band).max().unwrap();
            let first_min = (0..4).find(|&k| errs[k] == mn).unwrap();
            if in_band >= 2 && last_in != first_min {
                ties_found += 1;
                cases.push(E4Case {
                    c: mine_c,
                    lo,
                    hi,
                    init: init_none(),
                    n: 16,
                    px,
                });
            }
        }
        let name = if ties_found >= 30 {
            "bc7_test_eval4way".to_string()
        } else {
            format!("bc7_test_eval4way[tie-mining:{ties_found}<30]")
        };
        let mut input: Vec<u32> = Vec::with_capacity(cases.len() * 104);
        let mut want: Vec<u32> = Vec::with_capacity(cases.len() * 44);
        for e in &cases {
            push_ccp(&mut input, &e.c);
            input.push(e.n as u32);
            push_init(&mut input, &e.init);
            for arr in [e.lo[0], e.lo[1], e.hi[0], e.hi[1]] {
                push_i32s(&mut input, &arr);
            }
            push_pixels(&mut input, &e.px);
            let out = probe::eval_4way(&e.c, e.lo, e.hi, &e.init, e.n, &e.px);
            push_ccout(&mut want, &out, "eval4way");
        }
        assert_eq!(input.len(), cases.len() * 104);
        self.check_as(
            g,
            "bc7_test_eval4way",
            name,
            &[],
            &input,
            &want,
            cases.len(),
            44,
        )
        .await
    }
}

fn findopt_mode_cfgs() -> Vec<(usize, u32, usize, u32, bool, bool, bool)> {
    vec![
        (0, 8, 1, 4, false, true, false),
        (1, 8, 1, 6, false, true, true),
        (2, 4, 0, 5, false, false, false),
        (3, 4, 0, 7, false, true, false),
        (4, 4, 0, 5, false, false, false),
        (4, 8, 1, 5, false, false, false),
        (5, 4, 0, 7, false, false, false),
        (6, 16, 2, 7, true, true, false),
        (6, 16, 2, 7, false, true, false),
        (7, 4, 0, 5, true, true, false),
    ]
}

#[derive(Clone)]
struct FOCase {
    mode: usize,
    c: probe::CCP,
    pbit_search: bool,
    xl: [f32; 4],
    xh: [f32; 4],
    init: probe::CCInit,
    n: usize,
    px: [[i32; 4]; 16],
}

impl Harness {
    async fn entry_findopt(&self, g: &Gpu) -> EntryResult {
        let sets = weight_sets();
        let mut st = 0xf1d0_0713_5eed_2222u64;
        let mut cases: Vec<FOCase> = Vec::new();
        for (mi, &(mode, nsw, tbl, comp_bits, has_alpha, has_pbits, share)) in
            findopt_mode_cfgs().iter().enumerate()
        {
            for pbit_search in [false, true] {
                for perceptual in [false, true] {
                    for (wi, &weights) in sets.iter().enumerate().take(3) {
                        for ci in 0..20usize {
                            let c = probe::CCP {
                                nsw,
                                tbl,
                                comp_bits,
                                weights,
                                has_alpha,
                                has_pbits,
                                share_pbit: share,
                                perceptual,
                            };
                            let mut blk = [0u8; 64];
                            gen_block(&mut st, mi * 7 + wi * 3 + ci, &mut blk);
                            let px = px_from_block(&blk);
                            let rf = |st: &mut u64| (xs64(st) % 1200) as f32 / 1024.0 - 0.05;
                            let mut xl = [rf(&mut st), rf(&mut st), rf(&mut st), rf(&mut st)];
                            let mut xh = [rf(&mut st), rf(&mut st), rf(&mut st), rf(&mut st)];
                            match ci % 5 {
                                0 => {
                                    for k in 0..4 {
                                        let mut lo = 255;
                                        let mut hi = 0;
                                        for row in &px {
                                            lo = lo.min(row[k]);
                                            hi = hi.max(row[k]);
                                        }
                                        xl[k] = lo as f32 / 255.0;
                                        xh[k] = hi as f32 / 255.0;
                                    }
                                }
                                1 => {
                                    xh = xl;
                                }
                                2 => {
                                    std::mem::swap(&mut xl, &mut xh);
                                }
                                3 => {
                                    xl = [-0.02, 0.0, 1.0, 1.04];
                                    xh = [1.03, 1.0, 0.0, -0.01];
                                }
                                _ => {}
                            }
                            let n = if ci % 3 == 0 {
                                1 + (xs64(&mut st) % 16) as usize
                            } else {
                                16
                            };
                            cases.push(FOCase {
                                mode,
                                c,
                                pbit_search,
                                xl,
                                xh,
                                init: init_none(),
                                n,
                                px,
                            });
                        }
                    }
                }
            }
        }
        let mut derived: Vec<FOCase> = Vec::new();
        for e in cases.iter().step_by(16) {
            let (_, out) =
                probe::find_optimal(e.mode, e.xl, e.xh, &e.c, e.pbit_search, &e.init, e.n, &e.px);
            let mut d = e.clone();
            d.init = probe::CCInit {
                err: out.0,
                low: out.1,
                high: out.2,
                pbits: out.3,
            };
            derived.push(d);
        }
        cases.extend(derived);
        let mut input: Vec<u32> = Vec::with_capacity(cases.len() * 98);
        let mut want: Vec<u32> = Vec::with_capacity(cases.len() * 46);
        for e in &cases {
            input.push(e.mode as u32);
            push_ccp(&mut input, &e.c);
            input.push(e.pbit_search as u32);
            input.push(e.n as u32);
            push_init(&mut input, &e.init);
            push_f32s(&mut input, &e.xl);
            push_f32s(&mut input, &e.xh);
            push_pixels(&mut input, &e.px);
            let (ret, out) =
                probe::find_optimal(e.mode, e.xl, e.xh, &e.c, e.pbit_search, &e.init, e.n, &e.px);
            push_u64(&mut want, ret);
            push_ccout(&mut want, &out, "findopt");
        }
        assert_eq!(input.len(), cases.len() * 98);
        self.check(g, "bc7_test_findopt", &input, &want, cases.len(), 46)
            .await
    }

    async fn entry_ccc(&self, g: &Gpu) -> EntryResult {
        let t = build_opt_tables();
        let opt_bytes = words_bytes(&opt_tables_words(&t));
        let sets = weight_sets();
        let mode_cfgs: Vec<(usize, u32, usize, u32, bool, bool, bool)> = vec![
            (0, 8, 1, 4, false, true, false),
            (1, 8, 1, 6, false, true, true),
            (2, 4, 0, 5, false, false, false),
            (3, 4, 0, 7, false, true, false),
            (4, 8, 1, 5, false, false, false),
            (5, 4, 0, 7, false, false, false),
            (6, 16, 2, 7, true, true, false),
            (7, 4, 0, 5, true, true, false),
        ];
        let cp_buckets: Vec<(bool, u32, u32, u32)> = vec![
            (true, 1, 0, 7),
            (false, 1, 1, 7),
            (true, 1, 1, 7),
            (true, 1, 2, 7),
            (true, 2, 4, 7),
            (false, 0, 2, 5),
            (true, 0, 4, 1),
            (false, 3, 3, 2),
        ];
        let mut st = 0xcccc_0ddb_a11a_5eedu64;
        let mut input: Vec<u32> = Vec::new();
        let mut want: Vec<u32> = Vec::new();
        let mut total = 0usize;
        for (mi, &(mode, nsw, tbl, comp_bits, has_alpha, has_pbits, share)) in
            mode_cfgs.iter().enumerate()
        {
            for (cpi, &(pbit_search, refinement_passes, uber_level, uber1_mask)) in
                cp_buckets.iter().enumerate()
            {
                for perceptual in [false, true] {
                    for refinement in [false, true] {
                        for ci in 0..16usize {
                            let weights = sets[(mi + cpi + ci) % sets.len()];
                            let c = probe::CCP {
                                nsw,
                                tbl,
                                comp_bits,
                                weights,
                                has_alpha,
                                has_pbits,
                                share_pbit: share,
                                perceptual,
                            };
                            let mut blk = [0u8; 64];
                            gen_block(&mut st, ci, &mut blk);
                            let mut px = px_from_block(&blk);
                            match ci {
                                9 => {
                                    for (i, row) in px.iter_mut().enumerate() {
                                        *row = if i % 2 == 0 {
                                            [0, 0, 0, 255]
                                        } else {
                                            [255, 255, 255, 255]
                                        };
                                    }
                                }
                                10 => {
                                    px = [[137, 42, 250, 9]; 16];
                                }
                                11 => {
                                    let base = px[0];
                                    for (i, row) in px.iter_mut().enumerate() {
                                        *row = base;
                                        row[3] = (i * 17) as i32;
                                    }
                                }
                                _ => {}
                            }
                            let n = if ci % 3 == 0 {
                                1 + (xs64(&mut st) % 16) as usize
                            } else {
                                16
                            };
                            input.push(mode as u32);
                            push_ccp(&mut input, &c);
                            input.push(pbit_search as u32);
                            input.push(refinement_passes);
                            input.push(uber_level);
                            input.push(uber1_mask);
                            input.push(refinement as u32);
                            input.push(n as u32);
                            push_pixels(&mut input, &px);
                            let out = probe::ccc(
                                mode,
                                &c,
                                pbit_search,
                                refinement_passes,
                                uber_level,
                                uber1_mask,
                                refinement,
                                n,
                                &px,
                                &t,
                            );
                            push_ccout(&mut want, &out, "ccc");
                            total += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(input.len(), total * 82);
        let x = [(2, &opt_bytes[..])];
        self.check_as(
            g,
            "bc7_test_ccc",
            "bc7_test_ccc".into(),
            &x,
            &input,
            &want,
            total,
            44,
        )
        .await
    }
}

impl Harness {
    async fn entry_classify(&self, g: &Gpu) -> EntryResult {
        let (blocks, want) = classify_cases();
        let buf: Vec<u8> = blocks.concat();
        let n = "bc7_test_classify";
        self.check_bytes(
            g,
            n,
            n.into(),
            &[],
            &buf,
            &words_bytes(&want),
            blocks.len(),
            4,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
fn push_est_case(
    input: &mut Vec<u32>,
    op: u32,
    mode: u32,
    w: [u32; 4],
    perc: bool,
    num: u32,
    max_sol: i32,
    idxs: &[i32; 16],
    px: &[[i32; 4]; 16],
) {
    input.push(op);
    input.push(mode);
    input.extend_from_slice(&w);
    input.push(perc as u32);
    input.push(num);
    input.push(max_sol as u32);
    for &q in idxs {
        input.push(q as u32);
    }
    push_pixels(input, px);
}

fn push_u64_padded(want: &mut Vec<u32>, v: u64, pad_to: usize, ctx: &str) {
    assert!(v < 1u64 << 31, "{ctx}: host err {v} out of proven band");
    want.push(v as u32);
    want.push((v >> 32) as u32);
    for _ in 0..pad_to - 2 {
        want.push(0);
    }
}

fn push_sol_list(want: &mut Vec<u32>, l: &probe::SolList) {
    let (idx, errs, len) = l;
    for i in 0..8 {
        want.push(idx[i]);
        want.push(errs[i] as u32);
        want.push((errs[i] >> 32) as u32);
    }
    want.push(*len as u32);
}

fn est_idx_sets() -> Vec<([i32; 16], usize)> {
    let mut sets = Vec::new();
    let mut ident = [0i32; 16];
    for (i, v) in ident.iter_mut().enumerate() {
        *v = i as i32;
    }
    sets.push((ident, 16));
    sets.push((ident, 1));
    sets.push((ident, 7));
    let mut rev = [0i32; 16];
    for (i, v) in rev.iter_mut().enumerate() {
        *v = 15 - i as i32;
    }
    sets.push((rev, 16));
    for p in [0usize, 13, 34, 63] {
        let (idx, tot) = probe::subset_idx2(p);
        for s in 0..2 {
            sets.push((idx[s], tot[s] as usize));
        }
    }
    for p in [0usize, 21, 63] {
        let (idx, tot) = probe::subset_idx3(p);
        for s in 0..3 {
            sets.push((idx[s], tot[s] as usize));
        }
    }
    sets
}

fn est_blocks(st: &mut u64) -> Vec<[[i32; 4]; 16]> {
    let mut v = Vec::new();
    for strat in 0..9 {
        let mut blk = [0u8; 64];
        gen_block(st, strat, &mut blk);
        v.push(px_from_block(&blk));
    }
    v.push([[97, 4, 210, 33]; 16]);
    let mut alpha_only = [[120, 50, 200, 0]; 16];
    for (i, row) in alpha_only.iter_mut().enumerate() {
        row[3] = (i * 16) as i32;
    }
    v.push(alpha_only);
    let mut quads = [[0i32; 4]; 16];
    for (i, row) in quads.iter_mut().enumerate() {
        let q = ((i / 8) * 2 + (i % 4) / 2) as i32;
        *row = [q * 80, 255 - q * 60, q * q * 20, 255];
    }
    v.push(quads);
    v
}

fn est_variants() -> Vec<Params> {
    let mut v = params4().to_vec();
    let mut p = Params::slow(false);
    p.op_max_mode13 = 4;
    p.op_max_mode0 = 8;
    p.op_max_mode2 = 2;
    p.al_max_mode7 = 6;
    p.mode67_weight_mul = [2, 3, 1, 5];
    p.weights = [37, 5, 11, 3];
    v.push(p);
    let mut p = Params::slow(true);
    p.op_max_mode13 = 2;
    p.op_max_mode0 = 64;
    p.op_max_mode2 = 16;
    p.al_max_mode7 = 0;
    p.max_partitions_mode = [1, 16, 33, 64, 0, 0, 0, 3];
    v.push(p);
    let mut p = Params::basic(false);
    p.op_max_mode13 = 3;
    p.al_max_mode7 = 1;
    p.max_partitions_mode = [64, 35, 64, 64, 0, 0, 0, 64];
    v.push(p);
    v
}

impl Harness {
    async fn entry_solid(&self, g: &Gpu) -> EntryResult {
        let t = build_opt_tables();
        let opt_bytes = words_bytes(&opt_tables_words(&t));
        let axis = [0usize, 1, 2, 3, 31, 32, 63, 64, 127, 128, 191, 254, 255];
        let cas = [0i32, 1, 127, 254, 255];
        let mut st = 0x5011_dca5_e5ee_d001u64;
        let mut input: Vec<u32> = Vec::new();
        let mut want: Vec<u8> = Vec::new();
        let mut total = 0usize;
        let mut push_case =
            |input: &mut Vec<u32>, want: &mut Vec<u8>, cr: usize, cg: usize, cb: usize, ca: i32| {
                input.extend_from_slice(&[cr as u32, cg as u32, cb as u32, ca as u32]);
                want.extend_from_slice(&probe::block_solid(cr, cg, cb, ca, &t));
            };
        for (i, &cr) in axis.iter().enumerate() {
            for (j, &cg) in axis.iter().enumerate() {
                for (k, &cb) in axis.iter().enumerate() {
                    let ca = cas[(i + j + k) % cas.len()];
                    push_case(&mut input, &mut want, cr, cg, cb, ca);
                    total += 1;
                }
            }
        }
        for _ in 0..1024 {
            let cr = (xs64(&mut st) % 256) as usize;
            let cg = (xs64(&mut st) % 256) as usize;
            let cb = (xs64(&mut st) % 256) as usize;
            let ca = (xs64(&mut st) % 256) as i32;
            push_case(&mut input, &mut want, cr, cg, cb, ca);
            total += 1;
        }
        let x = [(2, &opt_bytes[..])];
        let n = "bc7_test_solid";
        self.check_bytes(g, n, n.into(), &x, &words_bytes(&input), &want, total, 16)
            .await
    }

    async fn entry_est_partition(&self, g: &Gpu) -> EntryResult {
        let variants = est_variants();
        let sets = est_idx_sets();
        let wsets = weight_sets();
        let mut st = 0xe57a_7e5e_ed06_0001u64;
        let blocks_px = est_blocks(&mut st);
        let mut grand_total = 0u32;
        let zc = |input: &mut Vec<u32>, op: u32, mode: u32, ms: i32, px: &[[i32; 4]; 16]| {
            push_est_case(input, op, mode, [0; 4], false, 0, ms, &[0; 16], px)
        };
        for (vi, cp) in variants.iter().enumerate() {
            let mut input: Vec<u32> = Vec::new();
            let mut want: Vec<u32> = Vec::new();
            let mut total = 0usize;
            for mode in [0usize, 1, 2, 3, 6, 7] {
                zc(&mut input, 4, mode as u32, 0, &blocks_px[0]);
                let (w, nsw, tbl, perc) = probe::est_params(mode, cp);
                want.extend_from_slice(&w);
                want.push(nsw);
                want.push(tbl);
                want.push(perc as u32);
                want.extend([0u32; 18]);
                total += 1;
            }
            for mode in [0usize, 1, 2, 3, 7] {
                for px in &blocks_px {
                    zc(&mut input, 2, mode as u32, 0, px);
                    want.push(probe::estimate_partition(mode, cp, px));
                    want.extend([0u32; 24]);
                    total += 1;
                }
            }
            for mode in [0usize, 1, 2, 3, 7] {
                for &ms in &[0i32, 1, 2, 3, 4, 6, 8, 16, 64] {
                    if ms == 0 && (mode == 0 || mode == 2) && cp.max_partitions_mode[mode] > 1 {
                        continue;
                    }
                    for px in blocks_px.iter().step_by(3) {
                        zc(&mut input, 3, mode as u32, ms, px);
                        push_sol_list(&mut want, &probe::estimate_partition_list(mode, cp, ms, px));
                        total += 1;
                    }
                }
            }
            if vi == 0 {
                for mode in [0usize, 1, 2, 3] {
                    for (si, (idxs, num)) in sets.iter().enumerate() {
                        for (wi, &w) in wsets.iter().enumerate() {
                            let px = &blocks_px[(si + wi + mode) % blocks_px.len()];
                            push_est_case(
                                &mut input,
                                0,
                                mode as u32,
                                w,
                                false,
                                *num as u32,
                                0,
                                idxs,
                                px,
                            );
                            let e = probe::est_idx(mode, w, idxs, *num, px);
                            push_u64_padded(&mut want, e, 25, "est_idx");
                            total += 1;
                        }
                    }
                }
                for perc in [false, true] {
                    for (si, (idxs, num)) in sets.iter().enumerate() {
                        for (wi, &w) in wsets.iter().enumerate() {
                            let px = &blocks_px[(si + 2 * wi + 1) % blocks_px.len()];
                            push_est_case(&mut input, 1, 7, w, perc, *num as u32, 0, idxs, px);
                            let e = probe::est_mode7_idx(w, perc, idxs, *num, px);
                            push_u64_padded(&mut want, e, 25, "est_mode7_idx");
                            total += 1;
                        }
                    }
                }
            }
            assert_eq!(input.len(), total * 89);
            assert_eq!(want.len(), total * 25);
            grand_total += total as u32;
            let pbytes = words_bytes(&params_words(cp));
            let r = self
                .check_as(
                    g,
                    "bc7_test_est_partition",
                    format!("bc7_test_est_partition[variant {vi}]"),
                    &[(1, &pbytes)],
                    &input,
                    &want,
                    total,
                    25,
                )
                .await;
            if !r.pass {
                return r;
            }
        }
        EntryResult {
            entry: "bc7_test_est_partition".into(),
            cases: grand_total,
            pass: true,
            first_diff: None,
        }
    }
}

fn plans_variants() -> Vec<Params> {
    let mut v = params4().to_vec();
    let mut p = Params::slow(false);
    p.op_max_mode13 = 4;
    p.op_max_mode0 = 2;
    p.op_max_mode2 = 8;
    p.al_max_mode7 = 6;
    v.push(p);
    let mut p = Params::slow(true);
    p.use_mode = [true, false, true, false, true, true, true];
    p.use_mode7 = false;
    p.op_max_mode0 = 3;
    v.push(p);
    let mut p = Params::basic(false);
    p.use_mode = [false, true, false, true, true, true, true];
    p.op_max_mode13 = 2;
    p.max_partitions_mode = [16, 1, 64, 64, 0, 0, 0, 64];
    v.push(p);
    let mut p = Params::slow(false);
    p.al_max_mode7 = 1;
    p.op_max_mode2 = 6;
    p.mode67_weight_mul = [3, 1, 2, 4];
    v.push(p);
    v
}

impl Harness {
    async fn entry_plans(&self, g: &Gpu) -> EntryResult {
        let mut st = 0x91a5_0000_0bad_5eedu64;
        let mut blocks_px = est_blocks(&mut st);
        for strat in 0..12 {
            let mut blk = [0u8; 64];
            gen_block(&mut st, strat, &mut blk);
            blocks_px.push(px_from_block(&blk));
        }
        let mut grand_total = 0u32;
        for (vi, cp) in plans_variants().iter().enumerate() {
            let mut input: Vec<u32> = Vec::new();
            let mut want: Vec<u32> = Vec::new();
            for px in &blocks_px {
                push_pixels(&mut input, px);
                let plan = probe::build_plans(cp, px);
                want.push(plan.part0);
                want.push(plan.part13);
                want.push(plan.part2);
                want.push(plan.use_list13 as u32);
                want.push(plan.use_list2 as u32);
                want.push(plan.use_list0 as u32);
                push_sol_list(&mut want, &plan.list13);
                push_sol_list(&mut want, &plan.list2);
                push_sol_list(&mut want, &plan.list0);
                push_sol_list(&mut want, &plan.list7);
            }
            let total = blocks_px.len();
            assert_eq!(input.len(), total * 64);
            assert_eq!(want.len(), total * 106);
            grand_total += total as u32;
            let pbytes = words_bytes(&params_words(cp));
            let r = self
                .check_as(
                    g,
                    "bc7_test_plans",
                    format!("bc7_test_plans[variant {vi}]"),
                    &[(1, &pbytes)],
                    &input,
                    &want,
                    total,
                    106,
                )
                .await;
            if !r.pass {
                return r;
            }
        }
        EntryResult {
            entry: "bc7_test_plans".into(),
            cases: grand_total,
            pass: true,
            first_diff: None,
        }
    }
}

const BF_SIZES: [(u32, u32); 3] = [(64, 64), (128, 32), (37, 53)];
const BF_SEEDS: [u64; 2] = [1, 7];

fn lin_cpu(tex: &[u8], srgb: bool) -> Vec<f32> {
    let n = tex.len() / 4;
    let mut out = vec![0f32; n * 4];
    for i in 0..n {
        mips::linearize_pixel(&tex[i * 4..i * 4 + 4], srgb, &mut out[i * 4..i * 4 + 4]);
    }
    out
}

fn f32s_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn u64s_bytes(v: &[u64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn pyramid(tex: &[u8], w: u32, h: u32, srgb: bool) -> Vec<(Vec<f32>, usize, usize)> {
    let mut levels = vec![(lin_cpu(tex, srgb), w as usize, h as usize)];
    loop {
        let (cur, w, h) = levels.last().unwrap();
        if *w == 1 && *h == 1 {
            break;
        }
        let (next, nw, nh) = mips::box_halve(cur, *w, *h);
        levels.push((next, nw, nh));
    }
    levels
}

impl Harness {
    async fn entry_blockify_linearize(&self, g: &Gpu) -> EntryResult {
        let pipeline = prepare_kernel_const(g, &self.blockify, "blockify_linearize", &[]);
        let mut cases_total = 0u32;
        for &(w, h) in &BF_SIZES {
            for srgb in [false, true] {
                let texs: Vec<Vec<u8>> = BF_SEEDS.iter().map(|&s| gen_texture(s, w, h)).collect();
                let npx = (w as u64) * (h as u64);
                let mut items = Vec::new();
                let mut prefixes = Vec::new();
                let mut base = Vec::new();
                let mut want = Vec::new();
                for (i, tex) in texs.iter().enumerate() {
                    items.extend(super::lin_item_bytes(i as u64 * npx, i as u64 * npx, srgb));
                    prefixes.push(i as u64 * npx);
                    base.extend_from_slice(tex);
                    want.extend(f32s_bytes(&lin_cpu(tex, srgb)));
                }
                let total = (npx as u32) * texs.len() as u32;
                let pyr = vec![0u8; total as usize * 16];
                cases_total += total;
                let name = format!("blockify_linearize[{w}x{h} srgb={srgb}]");
                let got = match dispatch_prepared_wg_pad(
                    g,
                    &pipeline,
                    total,
                    texs.len() as u32,
                    &[
                        (1, &items),
                        (4, &u64s_bytes(&prefixes)),
                        (5, &base),
                        (6, &pyr),
                    ],
                    6,
                    WG,
                    0,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => return error_result(&name, total, &e),
                };
                let r = compare(name, total, 16, &got, &want);
                if !r.pass {
                    return r;
                }
            }
        }
        EntryResult {
            entry: "blockify_linearize".into(),
            cases: cases_total,
            pass: true,
            first_diff: None,
        }
    }

    async fn entry_blockify_halve(&self, g: &Gpu) -> EntryResult {
        let pipeline = prepare_kernel_const(g, &self.blockify, "blockify_halve", &[]);
        let mut cases_total = 0u32;
        for &(w0, h0) in &BF_SIZES {
            for srgb in [false, true] {
                let mut curs: Vec<Vec<f32>> = BF_SEEDS
                    .iter()
                    .map(|&s| lin_cpu(&gen_texture(s, w0, h0), srgb))
                    .collect();
                let mut w = w0 as usize;
                let mut h = h0 as usize;
                let mut level = 0usize;
                while w > 1 || h > 1 {
                    let wants: Vec<(Vec<f32>, usize, usize)> =
                        curs.iter().map(|c| mips::box_halve(c, w, h)).collect();
                    let (nw, nh) = (wants[0].1, wants[0].2);
                    let px = (w * h) as u64;
                    let np = (nw * nh) as u64;
                    let nsrc = curs.len() as u64;
                    let mut pyr_f: Vec<f32> = Vec::new();
                    let mut items = Vec::new();
                    let mut prefixes = Vec::new();
                    for (i, cur) in curs.iter().enumerate() {
                        pyr_f.extend_from_slice(cur);
                        items.extend(super::halve_item_bytes(
                            i as u64 * px,
                            nsrc * px + i as u64 * np,
                            w as u32,
                            h as u32,
                        ));
                        prefixes.push(i as u64 * np);
                    }
                    pyr_f.extend(std::iter::repeat_n(0f32, (nsrc * np) as usize * 4));
                    let total = (np * nsrc) as u32;
                    cases_total += total;
                    let ctx = format!("blockify_halve[{w0}x{h0} srgb={srgb} level={level}]");
                    let got = match dispatch_prepared_wg_pad(
                        g,
                        &pipeline,
                        total,
                        curs.len() as u32,
                        &[
                            (3, &items),
                            (4, &u64s_bytes(&prefixes)),
                            (6, &f32s_bytes(&pyr_f)),
                        ],
                        6,
                        WG,
                        0,
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => return error_result(&ctx, total, &e),
                    };
                    for (i, (want, _, _)) in wants.iter().enumerate() {
                        let off = ((nsrc * px + i as u64 * np) * 4 * 4) as usize;
                        let want_b = f32s_bytes(want);
                        let name = format!(
                            "blockify_halve[{w0}x{h0} srgb={srgb} level={level} seed={}]",
                            BF_SEEDS[i]
                        );
                        let mut r =
                            compare(name, total, 16, &got[off..off + want_b.len()], &want_b);
                        if !r.pass {
                            if let Some(d) = r.first_diff.as_mut() {
                                d.byte_offset += off as u32;
                            }
                            return r;
                        }
                    }
                    curs = wants.into_iter().map(|(v, _, _)| v).collect();
                    w = nw;
                    h = nh;
                    level += 1;
                }
            }
        }
        EntryResult {
            entry: "blockify_halve".into(),
            cases: cases_total,
            pass: true,
            first_diff: None,
        }
    }

    async fn entry_blockify_quantize_pack(&self, g: &Gpu) -> EntryResult {
        let pipeline = prepare_kernel_const(g, &self.blockify, "blockify_quantize_pack", &[]);
        let mut cases_total = 0u32;
        for &(w0, h0) in &BF_SIZES {
            for srgb in [false, true] {
                let mut pyr_f: Vec<f32> = Vec::new();
                let mut items = Vec::new();
                let mut prefixes = Vec::new();
                let mut want = Vec::new();
                let mut lvl_px = 0u64;
                let mut blk_off = 0u64;
                let mut total_blocks = 0u64;
                let mut n_items = 0u32;
                for &seed in &BF_SEEDS {
                    let tex = gen_texture(seed, w0, h0);
                    for (level, w, h) in pyramid(&tex, w0, h0, srgb) {
                        let (bw, bh) = mips::level_block_dims(w, h);
                        items.extend(super::pack_item_bytes(
                            lvl_px, blk_off, w as u32, h as u32, srgb,
                        ));
                        prefixes.push(total_blocks);
                        for by in 0..bh {
                            for bx in 0..bw {
                                let mut blk = [0u8; 64];
                                mips::quantize_pack_block(&level, w, h, srgb, bx, by, &mut blk);
                                want.extend_from_slice(&blk);
                            }
                        }
                        pyr_f.extend_from_slice(&level);
                        lvl_px += (w * h) as u64;
                        blk_off += (bw * bh) as u64;
                        total_blocks += (bw * bh) as u64;
                        n_items += 1;
                    }
                }
                let blocks = vec![0u8; total_blocks as usize * 64];
                cases_total += total_blocks as u32;
                let name = format!("blockify_quantize_pack[{w0}x{h0} srgb={srgb}]");
                let got = match dispatch_prepared_wg_pad(
                    g,
                    &pipeline,
                    total_blocks as u32,
                    n_items,
                    &[
                        (2, &items),
                        (4, &u64s_bytes(&prefixes)),
                        (6, &f32s_bytes(&pyr_f)),
                        (7, &blocks),
                    ],
                    7,
                    WG,
                    0,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => return error_result(&name, total_blocks as u32, &e),
                };
                let r = compare(name, total_blocks as u32, 64, &got, &want);
                if !r.pass {
                    return r;
                }
            }
        }
        EntryResult {
            entry: "blockify_quantize_pack".into(),
            cases: cases_total,
            pass: true,
            first_diff: None,
        }
    }
}

const ENC_CASE_CAP: usize = 128;

fn texture_blocks(tex: &[u8], w: u32, h: u32, srgb: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for (level, lw, lh) in pyramid(tex, w, h, srgb) {
        let (bw, bh) = mips::level_block_dims(lw, lh);
        for by in 0..bh {
            for bx in 0..bw {
                let mut blk = [0u8; 64];
                mips::quantize_pack_block(&level, lw, lh, srgb, bx, by, &mut blk);
                out.extend_from_slice(&blk);
            }
        }
    }
    out
}

fn hint_code(cp: &Params, px: &[[i32; 4]; 16]) -> (u32, Params) {
    let (applied, gated) = probe::mode_tree_hint(px, cp);
    let code = if !applied {
        0
    } else if !gated.use_mode6 {
        1
    } else {
        2
    };
    (code, gated)
}

fn extreme_blocks() -> Vec<u8> {
    let mut out = Vec::new();
    let (cls, _) = classify_cases();
    for b in &cls {
        out.extend_from_slice(b);
    }
    for px in [
        [0u8, 0, 0, 0],
        [255, 255, 255, 255],
        [0, 0, 0, 255],
        [255, 255, 255, 0],
        [1, 1, 1, 254],
        [128, 128, 128, 127],
    ] {
        out.extend_from_slice(&solid_block(px));
    }
    out.extend_from_slice(&block_with(|i| {
        [
            (i * 17) as u8,
            255 - (i * 16) as u8,
            (i * i) as u8,
            if i < 8 { 0 } else { 255 },
        ]
    }));
    out.extend_from_slice(&block_with(|i| [200, 10, 30, (i * 17) as u8]));
    out.extend_from_slice(&block_with(|i| {
        let v = (i * 16) as u8;
        [v, v, v, 254]
    }));
    out.extend_from_slice(&block_with(|i| {
        [255 - i as u8, i as u8, 128, 255 - (i as u8 & 1)]
    }));
    out.extend_from_slice(&block_with(|i| {
        [
            10 + i as u8,
            20,
            250 - i as u8,
            if i == 5 { 3 } else { 200 },
        ]
    }));
    out.extend_from_slice(&block_with(|i| [(i * 4) as u8, 0, 255, 1]));
    out
}

fn encode_buckets() -> Vec<(String, Params)> {
    let mut v: Vec<(String, Params)> = params4()
        .iter()
        .enumerate()
        .map(|(i, p)| (format!("bucket{i}"), p.clone()))
        .collect();
    let mut p = Params::basic(false);
    p.mode6_only = true;
    v.push(("mode6_only".into(), p));
    v
}

fn encode_blocks_cpu(cp: &Params, t: &OptTables, blocks: &[u8]) -> Vec<u8> {
    let num_blocks = blocks.len() / 64;
    let mut out = Vec::with_capacity(num_blocks * 16);
    for chunk in blocks.chunks(GROUP_WIDTH * 64) {
        let n = chunk.len() / 64;
        let mut grp = [[0u8; 16]; GROUP_WIDTH];
        encode_group(chunk, n, cp, t, &mut grp);
        for b in &grp[..n] {
            out.extend_from_slice(b);
        }
    }
    out
}

fn encode_cases() -> Vec<(String, Vec<u8>)> {
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    let sizes: &[(u32, u32, &[u64])] = &[
        (64, 64, &[1, 7, 11]),
        (128, 32, &[1, 7]),
        (37, 53, &[1, 7]),
        (256, 256, &[1]),
    ];
    for &(w, h, seeds) in sizes {
        for &seed in seeds {
            for srgb in [false, true] {
                cases.push((
                    format!("tex seed={seed} {w}x{h} srgb={srgb}"),
                    texture_blocks(&gen_texture(seed, w, h), w, h, srgb),
                ));
            }
        }
    }
    let solid_tex: Vec<u8> = std::iter::repeat_n([40u8, 80, 120, 200], 64 * 64)
        .flatten()
        .collect();
    cases.push((
        "all-solid 64x64".into(),
        texture_blocks(&solid_tex, 64, 64, false),
    ));
    let mut alpha_tex = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u32 {
        for x in 0..64u32 {
            alpha_tex.extend_from_slice(&[
                (x * 4) as u8,
                (y * 4) as u8,
                (x + y) as u8,
                ((x * 255) / 63) as u8,
            ]);
        }
    }
    cases.push((
        "alpha-gradient 64x64".into(),
        texture_blocks(&alpha_tex, 64, 64, false),
    ));
    cases.push(("handcrafted extremes".into(), extreme_blocks()));
    for (_, blocks) in cases.iter_mut() {
        if blocks.len() > ENC_CASE_CAP * 64 {
            blocks.truncate(ENC_CASE_CAP * 64);
        }
    }
    cases
}

fn set_plan_word(want: &mut [u32], mask: &mut [bool], i: usize, v: u32) {
    want[i] = v;
    mask[i] = true;
}

fn set_plan_list(want: &mut [u32], mask: &mut [bool], base: usize, l: &probe::SolList) {
    let (idx, errs, len) = l;
    for i in 0..8 {
        set_plan_word(want, mask, base + i * 3, idx[i]);
        set_plan_word(want, mask, base + i * 3 + 1, errs[i] as u32);
        set_plan_word(want, mask, base + i * 3 + 2, (errs[i] >> 32) as u32);
    }
    set_plan_word(want, mask, base + 24, *len as u32);
}

fn expected_plan(cp: &Params, blocks: &[u8]) -> (Vec<u32>, Vec<bool>) {
    let nb = blocks.len() / 64;
    let mut want = vec![0u32; nb * PLAN_STRIDE];
    let mut mask = vec![false; nb * PLAN_STRIDE];
    let mut pxs = Vec::with_capacity(nb);
    let mut clss = Vec::with_capacity(nb);
    for b in 0..nb {
        let blk: [u8; 64] = blocks[b * 64..(b + 1) * 64].try_into().unwrap();
        pxs.push(px_from_block(&blk));
        clss.push(group_signature(&blk, 1) as u32);
    }
    for gstart in (0..nb).step_by(GROUP_WIDTH) {
        let gn = GROUP_WIDTH.min(nb - gstart);
        let alpha_blocks: Vec<usize> = (gstart..gstart + gn).filter(|&b| clss[b] == 1).collect();
        let opaque_blocks: Vec<usize> = (gstart..gstart + gn)
            .filter(|&b| clss[b] == 2 && !cp.mode6_only)
            .collect();
        if !alpha_blocks.is_empty() && cp.use_mode7 {
            let lanes: Vec<&[[i32; 4]; 16]> = alpha_blocks.iter().map(|&b| &pxs[b]).collect();
            let lists = probe::estimate_partition_list_lanes(7, cp, cp.al_max_mode7 as i32, &lanes);
            for (k, &b) in alpha_blocks.iter().enumerate() {
                set_plan_list(&mut want, &mut mask, b * PLAN_STRIDE + 85, &lists[k]);
            }
        }
        if !opaque_blocks.is_empty() && (cp.use_mode[1] || cp.use_mode[3]) && cp.op_max_mode13 != 1
        {
            let lanes: Vec<&[[i32; 4]; 16]> = opaque_blocks.iter().map(|&b| &pxs[b]).collect();
            let lists =
                probe::estimate_partition_list_lanes(1, cp, cp.op_max_mode13 as i32, &lanes);
            for (k, &b) in opaque_blocks.iter().enumerate() {
                set_plan_word(&mut want, &mut mask, b * PLAN_STRIDE + 7, 1);
                set_plan_list(&mut want, &mut mask, b * PLAN_STRIDE + 10, &lists[k]);
            }
        }
    }
    for b in 0..nb {
        let px = &pxs[b];
        let cls = clss[b];
        let base = b * PLAN_STRIDE;
        set_plan_word(&mut want, &mut mask, base, cls);
        if cls == 1 {
            let mut lo_a = 255i32;
            let mut hi_a = 0i32;
            for row in px {
                lo_a = lo_a.min(row[3]);
                hi_a = hi_a.max(row[3]);
            }
            set_plan_word(&mut want, &mut mask, base + 1, lo_a as u32);
            set_plan_word(&mut want, &mut mask, base + 2, hi_a as u32);
            set_plan_word(&mut want, &mut mask, base + 3, hint_code(cp, px).0);
        } else if cls == 2 && !cp.mode6_only {
            if (cp.use_mode[1] || cp.use_mode[3]) && cp.op_max_mode13 == 1 {
                set_plan_word(
                    &mut want,
                    &mut mask,
                    base + 5,
                    probe::estimate_partition(1, cp, px),
                );
            }
            if cp.use_mode[0] {
                if cp.op_max_mode0 == 1 {
                    set_plan_word(
                        &mut want,
                        &mut mask,
                        base + 4,
                        probe::estimate_partition(0, cp, px),
                    );
                } else {
                    set_plan_word(&mut want, &mut mask, base + 9, 1);
                    set_plan_list(
                        &mut want,
                        &mut mask,
                        base + 60,
                        &probe::estimate_partition_list(0, cp, cp.op_max_mode0 as i32, px),
                    );
                }
            }
            if cp.use_mode[2] {
                if cp.op_max_mode2 == 1 {
                    set_plan_word(
                        &mut want,
                        &mut mask,
                        base + 6,
                        probe::estimate_partition(2, cp, px),
                    );
                } else {
                    set_plan_word(&mut want, &mut mask, base + 8, 1);
                    set_plan_list(
                        &mut want,
                        &mut mask,
                        base + 35,
                        &probe::estimate_partition_list(2, cp, cp.op_max_mode2 as i32, px),
                    );
                }
            }
        }
    }
    (want, mask)
}

fn plan_word_pass(o: usize) -> &'static str {
    match o {
        0..=3 | 85..=109 => "bc7_plan_alpha",
        5 | 7 | 10..=34 => "bc7_plan_opaque13",
        _ => "bc7_plan_opaque02",
    }
}

fn le_word(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

impl Harness {
    async fn entry_encode_group(&self, g: &Gpu) -> Vec<EntryResult> {
        let fone = 1.0f32.to_bits();
        let t = build_opt_tables();
        let opt_bytes = words_bytes(&opt_tables_words(&t));
        let cases = encode_cases();
        let plan_pipes = ["bc7_plan_alpha", "bc7_plan_opaque13", "bc7_plan_opaque02"]
            .map(|e| prepare_kernel_const(g, &self.module, e, &[]));
        let enc_pipes = [0.0f64, 1.0, 2.0].map(|c| {
            prepare_kernel_const(g, &self.module, "bc7_encode_blocks", &[("TRIAL_CLASS", c)])
        });
        let mut results = Vec::new();
        'bucket: for (bname, cp) in encode_buckets() {
            let pbytes = words_bytes(&params_words(&cp));
            let mut bucket_blocks = 0u32;
            for (cname, blocks) in &cases {
                let nb = blocks.len() / 64;
                let num_groups = nb.div_ceil(GROUP_WIDTH) as u32;
                bucket_blocks += nb as u32;
                let want = encode_blocks_cpu(&cp, &t, blocks);
                let classes: Vec<u32> = (0..nb)
                    .map(|b| group_signature(&blocks[b * 64..(b + 1) * 64], 1) as u32)
                    .collect();
                let mut scratch = vec![0u8; nb * PLAN_STRIDE * 4];
                for (pi, pipe) in plan_pipes.iter().enumerate() {
                    match dispatch_prepared_wg_pad(
                        g,
                        pipe,
                        num_groups,
                        nb as u32,
                        &[(1, &pbytes), (4, blocks), (3, &scratch)],
                        3,
                        64,
                        fone,
                    )
                    .await
                    {
                        Ok(v) => scratch = v,
                        Err(e) => {
                            results.push(error_result(
                                &format!(
                                    "bc7_encode_group[{bname}][case {cname}][stage plan pass {pi}]"
                                ),
                                nb as u32,
                                &e,
                            ));
                            continue 'bucket;
                        }
                    }
                }
                let sw: Vec<u32> = scratch.chunks_exact(4).map(le_word).collect();
                let (pw, pm) = expected_plan(&cp, blocks);
                if let Some(i) = (0..sw.len()).find(|&i| pm[i] && sw[i] != pw[i]) {
                    let blk = i / PLAN_STRIDE;
                    let wo = i % PLAN_STRIDE;
                    results.push(EntryResult {
                        entry: format!(
                            "bc7_encode_group[{bname}][case {cname}][stage plan:{}][block {blk} class {}][plan word {wo}/{PLAN_STRIDE}]",
                            plan_word_pass(wo), classes[blk]
                        ),
                        cases: nb as u32,
                        pass: false,
                        first_diff: Some(FirstDiff {
                            byte_offset: (i * 4) as u32,
                            got_word: sw[i],
                            want_word: pw[i],
                            case_index: blk as u32,
                        }),
                    });
                    continue 'bucket;
                }
                let mut out = vec![0u8; nb * 16];
                for (ci, pipe) in enc_pipes.iter().enumerate() {
                    match dispatch_prepared_wg_pad(
                        g,
                        pipe,
                        nb as u32,
                        nb as u32,
                        &[
                            (1, &pbytes),
                            (2, &opt_bytes),
                            (4, blocks),
                            (5, &scratch),
                            (3, &out),
                        ],
                        3,
                        64,
                        fone,
                    )
                    .await
                    {
                        Ok(v) => out = v,
                        Err(e) => {
                            results.push(error_result(
                                &format!(
                                    "bc7_encode_group[{bname}][case {cname}][stage encode TRIAL_CLASS={ci}]"
                                ),
                                nb as u32,
                                &e,
                            ));
                            continue 'bucket;
                        }
                    }
                    for b in 0..nb {
                        let eff = if classes[b] == 2 && cp.mode6_only {
                            0
                        } else {
                            classes[b]
                        };
                        if eff as usize != ci {
                            continue;
                        }
                        let gb = &out[b * 16..b * 16 + 16];
                        let wb = &want[b * 16..b * 16 + 16];
                        if gb != wb {
                            let j = gb.iter().zip(wb.iter()).position(|(a, c)| a != c).unwrap();
                            let abs = b * 16 + j;
                            let wa = abs / 4 * 4;
                            results.push(EntryResult {
                                entry: format!(
                                    "bc7_encode_group[{bname}][case {cname}][stage encode TRIAL_CLASS={ci}][block {b} class {}][byte {j}/16]",
                                    classes[b]
                                ),
                                cases: nb as u32,
                                pass: false,
                                first_diff: Some(FirstDiff {
                                    byte_offset: abs as u32,
                                    got_word: le_word(&out[wa..wa + 4]),
                                    want_word: le_word(&want[wa..wa + 4]),
                                    case_index: b as u32,
                                }),
                            });
                            continue 'bucket;
                        }
                    }
                }
                if out != want {
                    let i = out
                        .iter()
                        .zip(want.iter())
                        .position(|(a, b)| a != b)
                        .unwrap();
                    let b = i / 16;
                    let wa = i / 4 * 4;
                    results.push(EntryResult {
                        entry: format!(
                            "bc7_encode_group[{bname}][case {cname}][stage final][block {b} class {}][byte {}/16]",
                            classes[b], i % 16
                        ),
                        cases: nb as u32,
                        pass: false,
                        first_diff: Some(FirstDiff {
                            byte_offset: i as u32,
                            got_word: le_word(&out[wa..wa + 4]),
                            want_word: le_word(&want[wa..wa + 4]),
                            case_index: b as u32,
                        }),
                    });
                    continue 'bucket;
                }
            }
            results.push(EntryResult {
                entry: format!("bc7_encode_group[{bname}][capped:{ENC_CASE_CAP} blocks/case]"),
                cases: bucket_blocks,
                pass: true,
                first_diff: None,
            });
        }
        results
    }
}

pub async fn run_bisect(g: &crate::gpu::wgpu::Gpu) -> Vec<EntryResult> {
    let module = g
        .device
        .create_shader_module(::wgpu::ShaderModuleDescriptor {
            label: Some("bc7-bisect"),
            source: ::wgpu::ShaderSource::Wgsl(BC7_WGSL.into()),
        });
    let blockify = g
        .device
        .create_shader_module(::wgpu::ShaderModuleDescriptor {
            label: Some("blockify-bisect"),
            source: ::wgpu::ShaderSource::Wgsl(BLOCKIFY_WGSL.into()),
        });
    let h = Harness { module, blockify };
    let t = build_opt_tables();
    let mut results: Vec<EntryResult> = Vec::new();
    results.extend(h.entry_tables(g).await);
    results.push(h.entry_u64ops(g).await);
    results.push(
        h.entry_pack(g, "bc7_test_pack_mode0", &[8], |_, p, w, rgba, n, px| {
            probe::pack_mode0_one_color(p, w, rgba, n, px, &t)
        })
        .await,
    );
    results.push(
        h.entry_pack(g, "bc7_test_pack_mode1", &[8], |_, p, w, rgba, n, px| {
            probe::pack_mode1_one_color(p, w, rgba, n, px, &t)
        })
        .await,
    );
    results.push(
        h.entry_pack(
            g,
            "bc7_test_pack_mode24",
            &[4, 8],
            |nsw, p, w, rgba, n, px| probe::pack_mode24_one_color(nsw, p, w, rgba, n, px, &t),
        )
        .await,
    );
    results.push(
        h.entry_pack(g, "bc7_test_pack_mode6", &[16], |_, p, w, rgba, n, px| {
            probe::pack_mode6_one_color(p, w, rgba, n, px, &t)
        })
        .await,
    );
    results.push(
        h.entry_pack(g, "bc7_test_pack_mode7", &[4], |_, p, w, rgba, n, px| {
            probe::pack_mode7_one_color(p, w, rgba, n, px, &t)
        })
        .await,
    );
    results.push(h.entry_classify(g).await);
    results.push(h.entry_div(g).await);
    results.push(h.entry_vecmath(g).await);
    results.push(
        h.entry_dist(g, "bc7_test_dist_rgb", |e1, e2, p, w| {
            probe::dist_rgb(e1, e2, p, w)
        })
        .await,
    );
    results.push(
        h.entry_dist(g, "bc7_test_dist_rgba", |e1, e2, p, w| {
            probe::dist_rgba(e1, e2, p, w)
        })
        .await,
    );
    results.push(h.entry_lsq(g, "bc7_test_lsq_rgb", 8).await);
    results.push(h.entry_lsq(g, "bc7_test_lsq_rgba", 8).await);
    results.push(h.entry_lsq(g, "bc7_test_lsq_a", 2).await);
    results.push(h.entry_fixdeg(g).await);
    results.push(h.entry_ccc(g).await);
    results.push(h.entry_solid(g).await);
    results.push(h.entry_est_partition(g).await);
    results.push(h.entry_evalsol(g).await);
    results.push(h.entry_eval4way(g).await);
    results.push(h.entry_findopt(g).await);
    results.push(h.entry_plans(g).await);
    results.push(h.entry_blockify_linearize(g).await);
    results.push(h.entry_blockify_halve(g).await);
    results.push(h.entry_blockify_quantize_pack(g).await);
    results.extend(h.entry_encode_group(g).await);
    results
}
