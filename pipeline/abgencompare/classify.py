"""Classification -> the six display labels (identical / imperceptible /
visible / stub / structural / fail), faithful to the campaign conventions
:

  1. render comparator: ppm = max-over-azimuths(px_all) * 1e6 / (W*H);
     identical = 0 px, imperceptible <= 200 ppm, visible > 200 ppm.
  2. AMNESTY: a `visible` glb/animated row whose Δ>8 ppm <= 200 relabels
     `imperceptible`, note 'amp-amnesty: Δ>8 ≤200ppm'.
  3. textures: corpusStub -> stub; identical-decode -> identical;
     visible + '(no pixel compare)' note -> structural; loadFail*/skipped/
     behaveDiff -> fail.
  4. headless glb/animated (no renders): byte-identical -> identical;
     pid/CAB-normalized structure identical -> imperceptible (explicit
     'headless struct-identical, no render compare' note); structure drift
     -> structural; dump failure -> fail.
"""
from . import AMNESTY_PPM, FAILY, FALLBACK_AREA, IMPERCEPTIBLE_PPM


def render_class(amp, kind):
    """amp = metrics.compare_shots output. -> (class, label, ppm, ppm8, maxd,
    px_per_angle, notes). Applies thresholds + the Δ>8 amnesty."""
    az = [v for k, v in amp.items() if k != "wh"]
    notes = []
    if not az:
        return "fail", "fail", None, None, None, None, ["render incomplete: no azimuth data"]
    if any(v == "dim-mismatch" for v in az):
        return "visible", "visible", None, None, None, None, ["render dimension mismatch"]
    wh = amp.get("wh")
    area = (wh[0] * wh[1]) if wh else FALLBACK_AREA
    px = max(v[0] for v in az)
    px8 = max(v[1] for v in az)
    maxd = max(v[3] for v in az)
    ppm = round(px * 1e6 / area, 1)
    ppm8 = round(px8 * 1e6 / area, 1)
    if px == 0:
        cls = "identical"
    elif ppm <= IMPERCEPTIBLE_PPM:
        cls = "imperceptible"
    else:
        cls = "visible"
    label = cls
    if label == "visible" and ppm8 <= AMNESTY_PPM:
        label = "imperceptible"
        notes.append("amp-amnesty: Δ>8 ≤200ppm")
    px_per_angle = {f"a{i}": v for i, v in enumerate(az)}
    return cls, label, ppm, ppm8, maxd, px_per_angle, notes


def texture_label(texcmp_row):
    """Raw texcmp row -> display label (campaign build-data.py rules)."""
    cls = texcmp_row.get("class", "")
    notes = [str(n) for n in (texcmp_row.get("notes") or [])]
    if texcmp_row.get("corpusStub"):
        return "stub"
    if cls in ("identical-decode", "identical"):
        return "identical"
    if cls == "imperceptible":
        return "imperceptible"
    if cls == "visible":
        return "structural" if any("(no pixel compare)" in n for n in notes) else "visible"
    return "fail"


def headless_glb_class(byte, struct):
    """-> (class, label, notes) for a glb/animated pair without renders."""
    if byte.get("identical"):
        return "identical", "identical", ["bundle byte-identical"]
    cls = struct.get("class")
    if cls in ("loadFailOurs", "loadFailUpstream"):
        return cls, "fail", [struct.get("error") or cls]
    if cls == "structIdentical":
        note = "headless: pid/CAB-normalized structure identical (no render compare)"
        if struct.get("pinnedDiffLines"):
            note += f"; manifest/metadata fields differ ({struct['pinnedDiffLines']} pinned lines)"
        return "structIdentical", "imperceptible", [note]
    if cls == "structDiff":
        n = struct.get("diffLines", 0)
        return (
            "structDiff",
            "structural",
            [f"headless: {n} normalized objdump line(s) differ "
             f"(up-only {struct.get('upOnly', 0)} / ours-only {struct.get('oursOnly', 0)}; "
             f"no render compare)"],
        )
    return "skipped", "fail", [struct.get("error") or "structure analysis unavailable"]


def is_faily(cls):
    return any(cls.startswith(f) for f in FAILY)
