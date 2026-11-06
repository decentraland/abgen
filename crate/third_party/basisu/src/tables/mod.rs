// Modified in dcl-one: the ETC1S per-format transcoders are removed; see
// patches/0003-remove-etc1s-transcoders.patch beside this crate.
//! Lookup tables used by the decoders. The static tables of precomputed ETC1S
//! transcode data are removed in this copy; what remains is built in code once
//! at first use.

// Tables built once at first use.
pub mod bc1;
