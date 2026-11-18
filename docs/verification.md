# Verification

Stage 2 deletes originals. This document defines exactly what "verified" means,
because a check that is not specified is not a guarantee.

## Why a definition is needed

The tempting wording is "the output looks the same". That is not a test. This
document replaces it with six independent checks, each of which can pass or fail
on its own, each of which is recorded per file.

## The six checks

### 1. Decode integrity

The output must decode end to end without error. A truncated AVIF often decodes
its first scan and fails later, so the check runs a full decode, not a header
probe.

- **Pass:** full decode returns success.
- **Fail:** any decoder error, or an output whose decoded pixel count differs
  from its declared dimensions.

### 2. Dimension match

Width and height must equal the source exactly, unless the policy explicitly
requested a resize, in which case they must equal the *requested* dimensions.

- **Pass:** `(w, h)` matches the expectation.
- **Fail:** any mismatch. A silent resize is always a bug.

### 3. Pixel similarity

For lossy output, "identical" is unavailable by definition. The check is a
similarity threshold, and the threshold is a number, not a feeling.

- Metric: **SSIM**, computed on the decoded image at full resolution.
- Default threshold: **0.98**.
- Records the actual score, so a batch that passes at 0.981 is distinguishable
  from one that passes at 0.999.

For lossless targets (JPEG XL lossless), the check is exact equality instead.

### 4. Colour fidelity

The most common silent failure. Two independent checks:

- **ICC presence matches.** If the source had an embedded profile, the output
  must have one. A missing profile is a fail even when SSIM passes, because the
  pixels are only correct under the source profile.
- **Rendered difference within tolerance.** When both carry profiles, decode
  both to sRGB and compare; the mean per-channel delta must stay under the
  configured tolerance.

### 5. Orientation

If the source carried an EXIF orientation other than 1, either:

- the rotation was **baked into the pixels** and the output's orientation tag is
  1, or
- the rotation tag was **carried over unchanged**.

Both are acceptable; silently dropping it is not. The check confirms one of the
two holds.

### 6. Provenance

The original file's BLAKE3 is recorded in the ledger alongside the output's, so
that at any later date the claim "this output came from that source" can be
re-checked rather than trusted.

## Cost

Check 3 is the expensive one: it decodes both images at full resolution. For 90k
images that is the dominant cost of the whole operation.

Policy options:

- `verify = "full"` — every file, every check.
- `verify = "sampled"` — every file gets checks 1, 2, 4, 5, 6; check 3 runs on a
  configurable sample per batch, plus on every high-risk file.
- `verify = "safety_only"` — checks 1, 2, 4, 5, 6. No pixel metric.

Deleting an original is irreversible, so the default is `full` for high-risk
batches and `sampled` for low-risk ones. A user may choose weaker verification
for a low-value batch, but must do so explicitly in the policy.

## Human review

Two modes, both recorded in the ledger:

- **`auto`** — pass/fail is decided by the checks alone.
- **`auto_then_manual_release`** — the checks run, results are grouped into
  batches, and a human releases batches one at a time. The tool never deletes
  on the strength of a pass alone; it waits for the release.

Manual review happens per **batch**, never per file, because per-file review of
90k files is not a workflow anyone completes.

## Known limits

- SSIM does not detect semantic changes — a re-encode that subtly alters a face
  can still score above 0.98. This is a property of every perceptual metric and
  is why the policy defaults are conservative rather than maximal.
- Colour comparison is done through the decoder's colour management. If the
  decoder ignores ICC (some do), the check degrades to a presence check only,
  and this is recorded in the ledger rather than silently assumed.
- Audio and video verification follow the same structure with duration,
  sample-rate, channel-count and acoustic-fingerprint checks in place of the
  pixel metrics. Not implemented in stage 1.
