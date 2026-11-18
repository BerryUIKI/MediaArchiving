# Architecture

## The shape of the problem

Three properties of real media libraries drive the whole design:

| Property | Consequence |
|---|---|
| Tens of thousands of files over a network share | the scan must be resumable and cached, not a single pass in memory |
| Decisions are made by a human, over days | decisions belong in a file, not in interactive prompts |
| Re-encoding is lossy and irreversible | nothing may be deleted without recorded evidence |

Everything below follows from those.

## Stage separation

```
        ┌───────────────────────────────────────────────┐
        │  stage 1 — no writes outside the run dir      │
        │                                               │
 path ─▶│  scan ──▶ inventory.sqlite                    │
        │              │                                │
        │              ▼                                │
        │  policy ──▶ analyse ──▶ analysis.json          │
        │              │                                │
        │              ▼                                │
        │           plan ──▶ plan.json ──▶ report.html  │
        └──────────────┬────────────────────────────────┘
                       │  (human reviews; edits policy; re-plans)
                       ▼
        ┌───────────────────────────────────────────────┐
        │  stage 2 — mutates the library                │
        │  apply ──▶ verify ──▶ originals ──▶ ledger    │
        └───────────────────────────────────────────────┘
```

The boundary is a hard one: stage 1 has no code path that writes outside the run
directory. That is what makes it safe to point at a 90k-file library on the first
run and see what it thinks.

## Run directory

```
runs/<name>/
  inventory.sqlite   every file: size, mtime, real container, dimensions,
                     metadata flags, BLAKE3, duplicate group, keeper flag
  policy.toml        the human decisions — reusable across runs
  analysis.json      per-file verdict + estimated saving + risk tier
  plan.json          review batches, each with a stable id
  report.html        the human-readable artefact
```

`policy.toml` is deliberately **outside** any per-run artefact: the same policy
file can be pointed at next month's batch, which is what makes this a workflow
rather than a one-off script.

## Scan: resumability and cost

The inventory is SQLite in WAL mode, written in batches of 2,000. An interrupted
scan keeps everything already probed; a re-run skips files whose size and mtime
are unchanged.

Probing is **header-level only** — no pixel decode. For images, dimensions come
from a header parse; EXIF/ICC/XMP are detected by scanning the first 128 KiB for
their markers; orientation is read from the EXIF block. This keeps the probe in
the millisecond range per file, which is the only way 90k files over SMB stays
practical.

### Duplicate detection

Hashing every file would mean reading the whole library. Instead:

1. Group by exact byte size — files with a unique size cannot be duplicates.
2. BLAKE3 only the files in groups of 2 or more.

On a real library this reduced the hashing pass from 90k files to ~8k.

The **keeper** of each duplicate group is chosen deterministically — shortest
path, then oldest mtime — so repeated runs agree with each other rather than
nondeterministically marking different copies.

## Estimate model

Real savings are unknown without encoding. Encoding everything to find out costs
as much as doing the work, so the plan phase models savings from **bits per
pixel** (bytes × 8 ÷ pixel count):

| bits/px | typical source | modelled AVIF saving |
|---|---|---|
| > 2.0 | near-lossless / camera | 65% |
| 1.0 – 2.0 | decent web quality | 50% |
| 0.5 – 1.0 | already re-compressed | 30% |
| < 0.5 | heavily re-compressed | 10% → skipped |

The model is intentionally conservative: a file modelled at 10% is reported as a
skip, because a re-encode that does not pay for its own risk is not worth doing.
The report states plainly that these are estimates and that a sample must be
encoded before bulk approval.

## Risk tiering

Risk answers "what could be lost", not "how big is it":

| Tier | Trigger | Why |
|---|---|---|
| High | embedded ICC profile | dropping it shifts colours |
| High | EXIF orientation ≠ 1 | rotation loss turns a set upside down |
| High | file ≥ 100 MiB | expensive to redo |
| Medium | XMP, or EXIF without ICC | metadata that may not survive |
| Low | none of the above | bulk-approvable |

## Batch composition

Two dimensions, in order:

1. **Risk tier.** Low-risk work is the bulk and can be approved in one sweep;
   high-risk work gets individual attention. Mixing them means either reviewing
   90k files or rubber-stamping the dangerous ones.
2. **Parent directory.** Inside a tier, files are packed by directory so a batch
   reads as "this figure set". Caps: 500 files or 2 GiB per batch.

Batch ids are stable across runs (`high-<dir>-001`) so a later
`ma apply --batch high-<dir>-001` means the same thing it meant when the report
was written.

## Intent

The single most consequential knob. The correct policy is a property of what the
library is *for*, not of the files:

- `archive` — personal storage; lossy transcoding allowed.
- `dataset` — training corpus; **lossy transcoding refused**, because the pixels
  are the data and a 40% saving is a corrupted dataset.
- `sharing` — compatibility beats size.

`intent` is why decisions live in a file: the same library, scanned twice with
different intents, should produce different plans, and neither is "wrong".

## Non-goals

- **Being an encoder.** Image/video encoding is delegated to libraries and
  `ffmpeg`.
- **Judging content.** The tool may decide a file is *redundant*; it does not
  decide a file is *worthless*. Decisions about keeping a file belong to the
  person who owns the library.
- **A GUI in stage 1.** The policy model is the interface; a GUI is a skin over
  it, and building the skin first would hard-code decisions the model has not
  settled yet.
