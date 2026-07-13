use anyhow::{anyhow, Result};

pub fn guard<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panic in build_bundle".to_string());
            Err(anyhow!("panic: {msg}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_passes_ok_and_catches_panic() {
        assert_eq!(guard(|| Ok(7)).unwrap(), 7);
        let err = guard::<()>(|| panic!("boom")).unwrap_err();
        assert!(err.to_string().contains("boom"));
    }
}
