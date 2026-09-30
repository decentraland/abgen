use anyhow::{anyhow, Result};

pub(super) const PRELUDE: &str = include_str!("prelude.js");
pub(super) const SCENE_THREAD_STACK: usize = 8 << 20;
pub(super) const TIMED_FRAMES: u32 = 90;
pub(super) const FRAME_DT_SECS: f64 = 100.0 / 3000.0;
pub(super) const FRAME_DT_MS: f64 = 100.0 / 3.0;

pub(super) fn cjs_wrap(code: &str) -> String {
    format!(
        ";(function (module, exports) {{ {code}\n}}).call(__abgen_module.exports, __abgen_module, __abgen_module.exports);"
    )
}

pub(super) trait EngineSession {
    fn eval(&mut self, code: &str) -> Result<(), String>;
    fn call_tick(&mut self, kind: &str, dt: f64) -> Result<(), String>;
    fn call_advance_clock(&mut self, ms: f64) -> Result<(), String>;
    fn pump(&mut self) -> Result<()>;
}

/// The scene author writes the eval error text; it ends up in lane errors and logs, so
/// keep a bounded prefix.
const MAX_EVAL_ERROR_CHARS: usize = 512;

fn bounded_error(e: String) -> String {
    match e.char_indices().nth(MAX_EVAL_ERROR_CHARS) {
        Some((cut, _)) => format!("{}… ({} chars)", &e[..cut], e.chars().count()),
        None => e,
    }
}

/// Returns the scene script's top-level eval error (bounded), which is not fatal here.
pub(super) fn drive(session: &mut dyn EngineSession, code: &str) -> Result<Option<String>> {
    session
        .eval(PRELUDE)
        .map_err(|e| anyhow!("prelude eval: {e}"))?;
    session.pump()?;
    let eval_error = session.eval(&cjs_wrap(code)).err().map(bounded_error);
    if let Some(e) = &eval_error {
        tracing::warn!("scene eval failed: {e}");
    }
    session.pump()?;
    tick(session, "start", 0.0)?;
    tick(session, "update", 0.0)?;
    for _ in 0..TIMED_FRAMES {
        if let Err(e) = session.call_advance_clock(FRAME_DT_MS) {
            tracing::warn!("advance clock failed: {e}");
        }
        tick(session, "update", FRAME_DT_SECS)?;
    }
    Ok(eval_error)
}

fn tick(session: &mut dyn EngineSession, kind: &str, dt: f64) -> Result<()> {
    if let Err(e) = session.call_tick(kind, dt) {
        tracing::warn!("scene {kind} tick failed: {e}");
    }
    session.pump()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_errors_are_cut_on_a_char_boundary() {
        assert_eq!(bounded_error("short".into()), "short");
        let long = "é".repeat(MAX_EVAL_ERROR_CHARS + 10);
        let cut = bounded_error(long);
        assert!(cut.starts_with(&"é".repeat(MAX_EVAL_ERROR_CHARS)));
        assert!(cut.ends_with(&format!("… ({} chars)", MAX_EVAL_ERROR_CHARS + 10)));
    }
}
