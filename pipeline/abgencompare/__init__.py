"""abgen-compare: generalized AB-parity run pipeline.

One run = one entity-set x one platform x two bundle sources (local abgen JIT
server vs upstream ab-cdn), analyzed headlessly (bytes / structure / texture
decode) and optionally rendered (Unity harness, 3 azimuths, pixel classifier).
Run dirs are immutable once complete; re-runs create new run dirs.
"""

VERSION = "0.1.0"

LABELS = ("identical", "imperceptible", "visible", "stub", "structural", "fail")

FAILY = ("loadFailOurs", "loadFailUpstream", "skipped", "behaveDiff")

IMPERCEPTIBLE_PPM = 200.0
AMNESTY_DELTA = 8
AMNESTY_PPM = 200.0
AMP_BANDS = (8, 32)
FALLBACK_AREA = 1024 * 1024

PLATFORMS = ("mac", "windows", "linux", "webgl")
