#[path = "../../kernel-ptx/src/core/bc7/mod.rs"]
pub mod bc7;
#[path = "../../kernel-ptx/src/core/mips.rs"]
pub mod mips;
#[path = "../../kernel-ptx/src/core/mode_tree.rs"]
pub mod mode_tree;

#[inline]
pub fn sqrtf(x: f32) -> f32 {
    x.sqrt()
}
