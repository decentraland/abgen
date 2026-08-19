#![allow(clippy::needless_range_loop)]
// Canonical copy lives in kernel-ptx (compiled to PTX for the GPU path);
// the CPU crate includes it verbatim so the mode tree cannot drift between
// the two encoders.
include!("../kernel-ptx/src/core/mode_tree_body.rs");
