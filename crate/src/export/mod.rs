pub mod convert;
pub mod wire;

pub use wire::{parse_input, Input, InputBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    Json = 0,
    Output = 1,
    Error = 2,
    Manifest = 3,
}

pub trait Sink {
    fn emit(&self, kind: Kind, bytes: &[u8]);

    fn emit_json(&self, v: serde_json::Value) {
        self.emit(Kind::Json, v.to_string().as_bytes());
    }

    fn emit_output(&self, name: &str, data: &[u8]) {
        let mut blob = Vec::with_capacity(8 + name.len() + data.len());
        blob.extend_from_slice(&(name.len() as u32).to_le_bytes());
        blob.extend_from_slice(name.as_bytes());
        blob.extend_from_slice(&(data.len() as u32).to_le_bytes());
        blob.extend_from_slice(data);
        self.emit(Kind::Output, &blob);
    }

    fn emit_error(&self, msg: &str) {
        self.emit(Kind::Error, msg.as_bytes());
    }
}

impl<T: Sink + ?Sized> Sink for &T {
    fn emit(&self, kind: Kind, bytes: &[u8]) {
        (**self).emit(kind, bytes);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HostInfo {
    pub manifest_version: &'static str,
    pub content_server_url: &'static str,
}

impl HostInfo {
    pub const fn new(manifest_version: &'static str, content_server_url: &'static str) -> Self {
        Self {
            manifest_version,
            content_server_url,
        }
    }
}

pub const OK: i32 = 0;
pub const ERR_MALFORMED_INPUT: i32 = 1;
pub const ERR_CONVERT_FAILED: i32 = 2;

pub fn run(request: &[u8], sink: &dyn Sink, host: HostInfo) -> i32 {
    let Some(input) = wire::parse_input(request) else {
        sink.emit_error("malformed input blob");
        return ERR_MALFORMED_INPUT;
    };
    run_parsed(input, sink, host)
}

#[cfg(not(target_arch = "wasm32"))]
fn arm_gpu_once() {
    static ARMED: std::sync::Once = std::sync::Once::new();
    ARMED.call_once(|| {
        if std::panic::catch_unwind(crate::arm_gpu_default).is_err() {
            eprintln!("abgen-gpu: GPU init panicked; continuing on CPU");
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn arm_gpu_once() {}

pub fn run_parsed(input: Input, sink: &dyn Sink, host: HostInfo) -> i32 {
    arm_gpu_once();
    if input.mode != 1 {
        if let Err(e) = crate::builder::require_templates() {
            sink.emit_error(&format!(
                "build templates unavailable ({}): {e:#}",
                crate::builder::template_source()
            ));
            return ERR_CONVERT_FAILED;
        }
    }
    let r = match input.mode {
        1 => convert::scan(input, sink),
        2 => convert::convert_only(input, sink),
        3 => convert::lod_only(input, sink),
        _ => convert::convert(input, sink, host),
    };
    match r {
        Ok(()) => OK,
        Err(e) => {
            sink.emit_error(&format!("{e:#}"));
            ERR_CONVERT_FAILED
        }
    }
}

#[derive(Default)]
pub struct CollectingSink {
    inner: std::sync::Mutex<Collected>,
}

#[derive(Default, Debug)]
pub struct Collected {
    pub events: Vec<String>,
    pub outputs: Vec<(String, Vec<u8>)>,
    pub errors: Vec<String>,
    pub manifest: Option<String>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn take(&self) -> Collected {
        match self.inner.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl Sink for CollectingSink {
    fn emit_output(&self, name: &str, data: &[u8]) {
        if let Ok(mut g) = self.inner.lock() {
            g.outputs.push((name.to_string(), data.to_vec()));
        }
    }

    fn emit(&self, kind: Kind, bytes: &[u8]) {
        let Ok(mut g) = self.inner.lock() else {
            return;
        };
        match kind {
            Kind::Json => g.events.push(String::from_utf8_lossy(bytes).into_owned()),
            Kind::Output => {
                if let Some((name, data)) = split_output(bytes) {
                    g.outputs.push((name, data));
                }
            }
            Kind::Error => g.errors.push(String::from_utf8_lossy(bytes).into_owned()),
            Kind::Manifest => g.manifest = Some(String::from_utf8_lossy(bytes).into_owned()),
        }
    }
}

pub fn split_output(blob: &[u8]) -> Option<(String, Vec<u8>)> {
    let name_len = u32::from_le_bytes(blob.get(0..4)?.try_into().ok()?) as usize;
    let name = String::from_utf8(blob.get(4..4 + name_len)?.to_vec()).ok()?;
    let off = 4 + name_len;
    let data_len = u32::from_le_bytes(blob.get(off..off + 4)?.try_into().ok()?) as usize;
    let data = blob.get(off + 4..off + 4 + data_len)?.to_vec();
    Some((name, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HOST: HostInfo = HostInfo::new("v-abgen-test", "test://inline");

    #[test]
    fn output_payload_roundtrips() {
        let sink = CollectingSink::new();
        sink.emit_output("abc_windows", &[7u8, 8, 9]);
        let got = sink.take();
        assert_eq!(got.outputs.len(), 1);
        assert_eq!(got.outputs[0].0, "abc_windows");
        assert_eq!(got.outputs[0].1, vec![7, 8, 9]);
    }

    #[test]
    fn malformed_input_is_an_error_event_not_a_panic() {
        let sink = CollectingSink::new();
        let code = run(&[0xff, 0x00], &sink, TEST_HOST);
        assert_eq!(code, ERR_MALFORMED_INPUT);
        let got = sink.take();
        assert_eq!(got.errors, vec!["malformed input blob".to_string()]);
        assert!(got.outputs.is_empty());
    }

    #[test]
    fn a_request_with_no_models_reports_it_and_still_returns_ok() {
        let blob = InputBuilder::new()
            .file(
                "scene.json",
                br#"{"scene":{"base":"0,0","parcels":["0,0"]}}"#.to_vec(),
            )
            .platform("windows")
            .build();
        let sink = CollectingSink::new();
        let code = run(&blob, &sink, TEST_HOST);
        assert_eq!(code, OK);
        let got = sink.take();
        assert!(
            got.errors.iter().any(|e| e.contains("no .glb/.gltf")),
            "expected the no-models error, got {:?}",
            got.errors
        );
    }

    #[test]
    fn scan_mode_reports_the_entity_without_converting() {
        let blob = InputBuilder::new()
            .file("a.glb", b"not-a-real-glb".to_vec())
            .platform("windows")
            .mode(1)
            .build();
        let sink = CollectingSink::new();
        assert_eq!(run(&blob, &sink, TEST_HOST), OK);
        let got = sink.take();
        assert!(got.events.iter().any(|e| e.contains(r#""ev":"entity""#)));
        assert!(got.outputs.is_empty(), "scan must not produce bundles");
    }
}
