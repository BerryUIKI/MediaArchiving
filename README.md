# MediaArchiving

> Scan a large media library, work out what is worth transcoding or archiving,
> and produce a plan a human can review before anything is touched.

MediaArchiving is the workflow around the encoder, not the encoder itself. It
answers three questions in order:

1. **What is in this library?** — every file probed by header, not by extension.
2. **What would be worth doing?** — per-file verdicts, estimated savings, risk tier.
3. **What exactly would be done?** — a batch manifest, reviewable and resumable.

Stage 1 (`scan → plan → report`) **never modifies a file**. Destructive work
lives in stage 2, which is not implemented yet.

---

## Why a workflow rather than a one-shot script

A library of 90k files over a network share cannot be curated in one sitting:

- Walking it takes hours; the tool must **resume**, not restart.
- The decisions are about *classes* of files ("all PNGs above 1 MP"), made by a
  human who may not be at the keyboard; they belong in a **policy file**, not a
  prompt.
- Re-encoding is lossy and irreversible, so every change needs an **evidence
  trail** (original hash, verification result) before the original is released.

Each of those is a design constraint, not a nicety.

---

## Quick start

```bash
# 1. Walk the library. Resumable; re-running only re-probes changed files.
ma scan X:\Classification\Y-Heterosexual --run runs/yhet

# 2. Edit the decisions.
$EDITOR runs/yhet/policy.toml

# 3. Analyse, plan, and write the HTML report.
ma plan --run runs/yhet

# 4. Read it.
start runs/yhet/report.html
```

`ma status --run runs/yhet` prints what a run holds without re-scanning.

---

## The three stages

### 1. Detection — `ma scan`

Files are probed by **magic bytes**, not extension, because media libraries are
full of mislabelled files. The probe records the real container, dimensions,
duration, and metadata flags (EXIF, ICC, XMP, orientation).

The inventory is SQLite: the scan is transactional, so an interrupted walk keeps
what it already learned.

Exact duplicates are found by hashing only files that **share a size** with
another file — the full-library hash pass would cost hours for no extra accuracy.

### 2. Format advice — the policy file

Every decision a human makes is written to `policy.toml`, with the trade-offs
documented in the comments:

```toml
intent = "archive"       # archive | dataset | sharing
[image]
target = "avif"          # avif | webp | jxl | keep
quality = 60
min_savings_pct = 15
```

`intent = "dataset"` **refuses lossy re-encoding outright**, because for a
training corpus the pixels *are* the data and a 40% saving is a corrupted
dataset. `intent = "sharing"` favours compatibility over size. This single knob
is why the policy exists: the right answer is not a property of the file, it is
a property of what the library is for.

### 3. Original-file handling — stage 2, not yet built

Planned modes, in increasing order of safety:

| Mode | Meaning | Reversible |
|---|---|---|
| `replace` | delete originals after transcoding | no |
| `verify_then_replace` | verify each output, then delete | no, but evidenced |
| `archive` | pack originals into an archive | yes |
| `archive_encrypted` | encrypted archive, key stored separately | yes (with key) |

"Verified" is defined precisely in [docs/verification.md](docs/verification.md),
because an undefined check is not a guarantee.

---

## What the report tells you

`report.html` is a single self-contained file — no CDN, no external assets, so it
opens from a network share or an offline machine. It shows:

- headline totals: files, size, candidates, **estimated** saving
- a per-format breakdown of where the savings actually are
- risk layering: high / medium / low, and why each file landed where it did
- the review batches, with ids that a future `ma apply --batch <id>` will consume
- files that must be fixed before they can be planned

### Estimates are labelled as estimates

Savings are modelled from **bits per pixel** — total bytes divided by pixel
count — rather than measured by encoding, because encoding 90k images to find out
is the same cost as doing the work. The model is deliberately conservative: a
JPEG that looks like it will save 10% is reported as a skip, because a
re-encode that does not pay for its own risk is a bad trade.

Every number in the report is a *plan*, not a *result*. A sample batch must be
encoded and verified before any bulk approval.

---

## Risk layering

Review batches are composed two-dimensionally:

1. **Risk first.** Low-risk files are the bulk and can be approved in one sweep.
   High-risk files get individual attention. Mixing them means either reviewing
   90k files or rubber-stamping the dangerous ones.
2. **Then directory.** Inside a tier, files are packed by parent directory so a
   batch reads as "this figure set", not "these 400 unrelated paths".

What makes a file high-risk: an embedded ICC profile (dropping it shifts
colours), a non-trivial EXIF orientation (rotation loss turns a set upside down),
or a large file (expensive to redo). None of these are about *size*.

---

## Format trade-offs

Encoded in the binary for the formats it currently models:

| | AVIF | WebP | JPEG XL |
|---|---|---|---|
| Saves vs JPEG | most | moderate | good |
| Browser support | good | best | poor |
| Encode speed | slow | fast | moderate |
| Lossless | — | — | yes |
| Main risk | old software cannot open it | smaller savings | ecosystem support |

Default is **AVIF with WebP as the fallback**, decided per library: content that
will be viewed on old devices should go to WebP. These facts are data, not code —
they belong in a table that can be updated when the ecosystem moves.

---

## Building

Requires Rust ≥ 1.80 and, on Windows, MSVC build tools (SQLite is compiled in).

```bash
cargo build --release
./target/release/ma --help
```

Video transcoding additionally requires `ffmpeg` and `ffprobe` on `PATH`.
Hardware encoding (NVENC/QSV/VideoToolbox) is used when the policy allows.

---

## Status

Stage 1 is implemented: `scan`, `plan`, `report`.

Stage 2 — `apply`, `verify`, `originals` — has interfaces and a state machine
sketched in the design docs but **no implementation**. It will not ship until
the verification story is solid, because the failure mode (irreversible loss of
originals) is exactly the one a curation tool must not have.

## License

MIT OR Apache-2.0
