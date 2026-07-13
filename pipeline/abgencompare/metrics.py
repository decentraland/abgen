"""THE pixel comparator (faithful port of compare-classes.py +
compute-amp-metrics.py):

  differing pixel = RGB int16 abs-diff, per-pixel MAX over channels, > 0
  amplitude bands per azimuth: [px_all, px_amp8(Δ>8), px_amp32(Δ>32),
                                maxΔ, meanΔ(nonzero, 1dp)]
  'dim-mismatch' sentinel when shapes differ; wh from the actual shots.
  single-shot reuse (same path both sides) counts [0,0,0,0,0.0] trivially.

numpy+PIL are used when importable; otherwise a stdlib PNG decoder handles
Unity's non-interlaced 8-bit RGB(A)/gray PNGs (slower, same numbers).
"""
import os
import struct
import zlib

try:
    import numpy as _np
    from PIL import Image as _Image

    _FAST = True
except ImportError:
    _np = None
    _FAST = False

def _paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def _png_rgb(path):
    """-> (w, h, bytes rgb) — pure stdlib."""
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"not a PNG: {path}")
    pos = 8
    w = h = None
    bitdepth = ctype = None
    idat = []
    plte = None
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        typ = data[pos + 4:pos + 8]
        chunk = data[pos + 8:pos + 8 + length]
        pos += 12 + length
        if typ == b"IHDR":
            w, h, bitdepth, ctype, _comp, _filt, interlace = struct.unpack(
                ">IIBBBBB", chunk
            )
            if bitdepth != 8 or interlace != 0:
                raise ValueError(f"unsupported PNG (bd={bitdepth} il={interlace}): {path}")
        elif typ == b"PLTE":
            plte = chunk
        elif typ == b"IDAT":
            idat.append(chunk)
        elif typ == b"IEND":
            break
    raw = zlib.decompress(b"".join(idat))
    nch = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    stride = w * nch
    out = bytearray(h * stride)
    prev = bytearray(stride)
    pos = 0
    for y in range(h):
        ft = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        if ft == 1:
            for i in range(nch, stride):
                line[i] = (line[i] + line[i - nch]) & 0xFF
        elif ft == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ft == 3:
            for i in range(stride):
                left = line[i - nch] if i >= nch else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif ft == 4:
            for i in range(stride):
                left = line[i - nch] if i >= nch else 0
                ul = prev[i - nch] if i >= nch else 0
                line[i] = (line[i] + _paeth(left, prev[i], ul)) & 0xFF
        out[y * stride:(y + 1) * stride] = line
        prev = line
    if ctype == 2:
        return w, h, bytes(out)
    rgb = bytearray(w * h * 3)
    if ctype == 6:
        for i in range(w * h):
            rgb[i * 3:i * 3 + 3] = out[i * 4:i * 4 + 3]
    elif ctype == 0:
        for i in range(w * h):
            v = out[i]
            rgb[i * 3:i * 3 + 3] = bytes((v, v, v))
    elif ctype == 4:
        for i in range(w * h):
            v = out[i * 2]
            rgb[i * 3:i * 3 + 3] = bytes((v, v, v))
    elif ctype == 3:
        for i in range(w * h):
            p = out[i] * 3
            rgb[i * 3:i * 3 + 3] = plte[p:p + 3]
    return w, h, bytes(rgb)


def amp_bands(up_path, ours_path):
    """-> (bands|None, (w,h)) — bands None means dim-mismatch; wh is the
    upstream side's dims in that case (matches compute-amp-metrics.py)."""
    if _FAST:
        ia = _np.asarray(_Image.open(up_path).convert("RGB"), dtype=_np.int16)
        ib = _np.asarray(_Image.open(ours_path).convert("RGB"), dtype=_np.int16)
        if ia.shape != ib.shape:
            return None, (ia.shape[1], ia.shape[0])
        d = _np.abs(ia - ib).max(axis=2)
        px_all = int((d > 0).sum())
        if px_all == 0:
            return [0, 0, 0, 0, 0.0], (ia.shape[1], ia.shape[0])
        return (
            [
                px_all,
                int((d > 8).sum()),
                int((d > 32).sum()),
                int(d.max()),
                round(float(d[d > 0].mean()), 1),
            ],
            (ia.shape[1], ia.shape[0]),
        )
    wa, ha, a = _png_rgb(up_path)
    wb, hb, b = _png_rgb(ours_path)
    if (wa, ha) != (wb, hb):
        return None, (wa, ha)
    px_all = px8 = px32 = maxd = 0
    total = 0
    n = wa * ha
    for i in range(n):
        j = i * 3
        d0 = a[j] - b[j]
        d1 = a[j + 1] - b[j + 1]
        d2 = a[j + 2] - b[j + 2]
        d = max(d0 if d0 >= 0 else -d0, d1 if d1 >= 0 else -d1, d2 if d2 >= 0 else -d2)
        if d > 0:
            px_all += 1
            total += d
            if d > 8:
                px8 += 1
                if d > 32:
                    px32 += 1
            if d > maxd:
                maxd = d
    if px_all == 0:
        return [0, 0, 0, 0, 0.0], (wa, ha)
    return [px_all, px8, px32, maxd, round(total / px_all, 1)], (wa, ha)


def compare_shots(up_shots, ours_shots):
    """Per-azimuth amplitude metrics for a rendered pair.

    -> {"a0": [...]|"dim-mismatch", ..., "wh": [W,H]} exactly like the
    campaign amp-metrics files (single-shot reuse -> zeros)."""
    out = {}
    wh = None
    for i, (u, o) in enumerate(zip(up_shots, ours_shots)):
        if os.path.abspath(u) == os.path.abspath(o):
            out[f"a{i}"] = [0, 0, 0, 0, 0.0]
            if wh is None:
                if _FAST:
                    wh = list(_Image.open(u).size)
                else:
                    w, h, _ = _png_rgb(u)
                    wh = [w, h]
            continue
        bands, dims = amp_bands(u, o)
        if bands is None:
            out[f"a{i}"] = "dim-mismatch"
            if wh is None:
                wh = list(dims)
        else:
            out[f"a{i}"] = bands
            wh = list(dims)
    if wh:
        out["wh"] = wh
    return out
