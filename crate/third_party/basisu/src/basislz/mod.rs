// Modified in dcl-one: the ETC1S per-format transcoders are removed; see
// patches/0003-remove-etc1s-transcoders.patch beside this crate.
//! BasisLZ entropy codec for the ETC1S source path: the Huffman decoder, the
//! bitwise stream decoder, the endpoint/selector codebook decode (with its
//! move-to-front selector history). The per-target transcoders that turned
//! ETC1S blocks into each GPU format are removed in this copy.

pub mod amf;
pub mod decoder;
pub mod etc1s;
pub mod huffman;
