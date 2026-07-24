# Atlas sampling rationale

Why the LOD atlas bake behaves the way it does.

## Linear light, premultiplied alpha

Every image downscale filters in linear light with premultiplied alpha.
Averaging sRGB-encoded values biases the result dark, because equal weight in
the encoded domain is not equal radiance; decoding to linear before the area
kernel keeps the filtered output radiometrically correct. Premultiplying by
alpha keeps the colour of fully transparent texels from bleeding into covered
ones, so cutout and blended edges downscale without dark fringes.

## sRGB colour factors

glTF colour factors are linear, and the atlas stores 8-bit sRGB texels that
the client decodes exactly once at sample time. Folding a factor into the
atlas therefore multiplies in linear light and re-encodes the product as
sRGB, so the single decode in the shader reproduces the intended radiance.

## Emissive folds into the albedo by default

The client's LOD shader samples only the base map array at LOD1 and beyond,
and metallic and smoothness floats apply per material. Any energy carried in
a separate texture plane is invisible at those levels, so the default bake
folds emissive into the albedo — glowing surfaces keep their light instead of
going dark. Extra texture planes are therefore opt-in, for consumers that
render LOD0 and can sample them.

## Area-weighted allocation, fused repeat bake

Atlas area is a fixed budget, and a tile's visual contribution scales with
the world surface that samples it, not with its source resolution. Rects are
therefore sized by the summed triangle area of the primitives that reference
each tile, so a scene's floor outbids a small decal of the same source size.
Repeating UVs bake fused: the source is prefiltered once to per-repeat
dimensions and then tiled, which bounds memory by the canvas cap, keeps tile
dimensions aligned to the UV period, and preserves the pattern's frequency at
any repeat count.

## The 256 default, 512 available

The atlas budget defaults to 256 with 512 available. At 256,
texture-dominated scenes halve their texture wire — 32% to 52% smaller —
while staying visually near-lossless at LOD distances. Text and signage need
512 to stay readable, so the budget is a per-scene choice rather than a
global constant.
