# chperf

Chrome DevTools Performance trace analyzer, command-line only. Parses `{ "traceEvents": [...] }` JSON and surfaces actionable performance insights.

Accepts plain `.json` and gzipped `.json.gz` traces — a single file, a glob
pattern (e.g. `'traces/*.json'`), or a directory (list / batch export).

Chrome DevTools trace JSON files are massive and impractical to read by hand. chperf structures and summarizes the trace, then exports it as Markdown. Feed the exported result directly to an LLM like Claude Code for instant bottleneck identification and improvement suggestions.

## Install

```sh
cargo install --path crates/chperf-cli
```

Requires Rust 2024 edition (1.85+).

## Usage

```sh
# Single trace analysis (prints a Markdown report to stdout)
chperf trace.json

# Gzipped traces are supported transparently (.json.gz)
chperf trace.json.gz --export

# Compare two traces
chperf before.json --compare after.json

# Windowed compare: PRE/SHOOT/POST of both traces side by side + inter-trace
# deltas (SHOOT and SHOOT−PRE per trace, then B−A for both)
chperf before.json --compare after.json --anchor shoot --delta

# Any inspect query runs on both traces in compare mode
chperf before.json --compare after.json --frames --gc

# Export as Markdown (to stdout)
chperf trace.json --export

# Export to file
chperf trace.json --export=report.md

# Self-contained HTML report (single file, SVG memory charts).
# With --compare it renders a paired A-vs-B report: comparison findings at
# the top, a shared A/B overview header, per-section tables for both traces,
# then the detailed compare tables.
chperf trace.json --html=report.html
chperf before.json --compare after.json --html=report.html

# Compare + export
chperf before.json --compare after.json --export

# Compare + summary only (PR-friendly)
chperf before.json --compare after.json --export --summary

# Manual CPU throttle override (auto-detected from trace metadata when available)
chperf trace.json --throttle 6

# Directory of traces: list them (pick a file to analyze further)
chperf traces/

# Batch-export every trace in a directory to `chperf-export-<name>.md` files
chperf traces/ --export

# ... as HTML and/or machine-readable JSON instead (or as well)
chperf traces/ --html --json --out-dir out/

# The positional trace also accepts a glob pattern (expanded like a directory)
chperf 'traces/*.json' --export --out-dir out/

# Glob matching is non-recursive: `**` matches within a single directory,
# exactly like `*` (a directory listing filtered by file name).
```

## Inspect

Granular CLI inspection for questions the summary can't answer — scoped to a
time window with `--around <ms>` + `--window <half_ms>` (ms from trace start).
Output is Markdown, ready to paste to an LLM.

```sh
# Where is the jank? Start with a timeline, or jump straight to the worst task.
chperf trace.json --timeline                     # busy% per time bucket
chperf trace.json --timeline --around 11877674 --window 2000
chperf trace.json --worst --stacks --top 10      # auto-anchor on longest RunTask

# Discovery: what event names / threads exist?
chperf trace.json --names --top 20
chperf trace.json --threads --top 20             # find the main/GPU thread tid

# List events by name, filtered by min duration/thread/process, sorted by duration
chperf trace.json --events RunTask --sort dur --top 10      # top spikes
chperf trace.json --events GPUTask --tid 10033             # only on a thread
chperf trace.json --events '^Fire' --regex                 # names as regex

# How bad is it? Duration distribution per event name.
chperf trace.json --events RunTask,FunctionCall --stats

# Within ±100ms around a timestamp (ms from trace start)
chperf trace.json --events GPUTask,RunTask --around 11877674 --window 100

# Aggregate CPU self-time for matching functions; --tid main targets main thread.
chperf trace.json --function render --tid main --around 11877674 --window 100
chperf trace.json --function '^_render' --regex

# Heaviest call stacks (root → leaf) with full caller chain.
chperf trace.json --stacks --function render --around 11877674 --window 100

# Folded stacks → flamegraph.pl / speedscope.
chperf trace.json --flame --function render | flamegraph.pl > flame.svg

# What's inside the jank? Break down a RunTask's child events.
chperf trace.json --worst --task --top 5
chperf trace.json --task --around 11877674 --window 500 --top 3

# Search event args (substring or regex); --full-args disables truncation.
chperf trace.json --find setPlayerRespawned --top 20 --full-args
chperf trace.json --find 'frame.*sampleTraceId' --regex

# Machine-readable JSON for jq / pipelines (every inspector supports it;
# durations in µs, args emitted as parsed objects).
chperf trace.json --function render --top 10 --json | jq '.sections.functions[0]'
chperf trace.json --events FireAnimationFrame --json | jq '.sections.events[].args'

# Jank clusters: dropped frames / sub-threshold spikes hidden by the summary
chperf trace.json --jank

# Memory timeline: JS heap / DOM nodes / documents / listeners over time,
# with peak, growth velocity, and significant change points (leak check).
# --memory --json caps the sample list at --top rows (default 30) and notes
# the remainder as `"truncated": N`; the summary always covers all samples.
chperf trace.json --memory
chperf trace.json --memory --around 5000 --window 2000 --top 100 --json | jq '.sections.memory.summary'

# Input & media latency by event type (EventDispatch): categories (Input/Media/DOM),
# cadence / frequency (Hz), first-vs-last-5s activity, percentiles + worst events.
chperf trace.json --input
chperf trace.json --input --top 10

# Inter-event gaps and cadence/periodicity: detects timer loops, audio/video seek loops,
# RAF cadence, gap distribution histogram, jitter, and event sequences.
chperf trace.json --events seeking --gaps
chperf trace.json --events RunTask --gaps

# Browser frame/document hierarchy from TracingStartedInBrowser:
# reveals iframes, embedded SVGs, and document topology across processes.
chperf trace.json --frame-tree

# Generic Counter (ph C) series: `--counters` lists counters with sample
# counts; `--counters fps` renders the series table (t, value) + min/max/last.
chperf trace.json --counters
chperf trace.json --counters fps --json | jq '.sections.counters'

# Async task timings (s/f events paired by id): RAF, GC jobs, timers, …
chperf trace.json --async
chperf trace.json --async --top 20 --json | jq '.sections.async.longest'

# Async flow/causality chains (`s`/`t`/`f`/`n` + legacy `b`/`e` with ids,
# grouped by pid+id): per-name summary (count, avg/p95, unpaired) plus the
# longest chains with their steps. Optional case-insensitive name filter.
# Traces without async/flow events render an explicit none-found.
chperf trace.json --flows
chperf trace.json --flows loader --top 10 --json | jq '.sections.flows.longest'

# Per-renderer overview for multi-process traces (OOPIF/iframes/workers):
# full reports already carry a Renderers section + `renderers` JSON key.
chperf trace.json --renderers
chperf trace.json --renderers --json | jq '.sections.renderers'

# ── Windowed analysis around a semantic anchor (shoot/pre/post) ──
# --anchor finds the first match in FunctionCall functionName → CPU profile
# function/URL → event args; the SHOOT window = anchor ± --window (default
# ±100ms). --delta compares PRE (before) / SHOOT / POST (after) windows:
# frames, dropped frames, GC, long tasks, CPU samples, busy time.
chperf trace.json --anchor shoot --delta
chperf trace.json --anchor weapon.ts --window 250 --stacks --top 10

# Per-frame stats (b/e-paired frame events) + dropped frames, per window.
chperf trace.json --frames
chperf trace.json --anchor shoot --frames --window 100

# Inclusive CPU call tree (self + subtree time), pruned to a URL/file or
# function; ancestors of matches are kept so the path stays visible.
chperf trace.json --calltree --url weapon.ts --top 40

# GC + long tasks report for the window (--lt sets the long-task threshold).
chperf trace.json --gc --anchor shoot
chperf trace.json --gc --lt 30

# Combined search: event args AND CPU profile function names/URLs.
chperf trace.json --find shoot

# CSV output (every inspector supports it; --json for JSON)
chperf trace.json --frames --csv | cut -d, -f1,2

# Flags compose: combine --names + --stacks + --function in one run
```

| Flag | Purpose |
|------|---------|
| `--timeline` | Busy timeline (RunTask per time bucket, with bars) |
| `--worst` | Auto-anchor `--around` on the longest RunTask |
| `--names` | List distinct event names with count/total duration |
| `--threads` | List distinct threads (tid) with RunTask time and top event |
| `--stacks` | Heaviest CPU call stacks (root → leaf) with caller chains |
| `--flame` | Folded stacks (`a;b;c <us>`) for flamegraph.pl / speedscope |
| `--task` | Break down the heaviest RunTasks into child events + top JS call |
| `--events <a,b,…>` | List events matching these names |
| `--stats` | Duration distribution (min/avg/p50/p90/p99/max) per event name |
| `--function <pat>` | Aggregate CPU samples whose function name matches |
| `--find <pat>` | Search event `args` JSON **and** CPU profile names/URLs |
| `--calltree` | Inclusive CPU call tree (self + subtree), prune with `--function`/`--url` |
| `--url <pat>` | Restrict CPU functions/stacks/calltree to source URLs matching |
| `--gc` | GC (major/minor/other) + long-task report for the window |
| `--lt <ms>` | Long-task threshold in ms (default 50, with `--gc`/`--delta`) |
| `--frames` | Per-frame duration stats (b/e-paired) + dropped frames |
| `--frame-event <name>` | Frame event for `--frames`/`--delta` (default `SubmitCompositorFrameToPresentationCompositorFrame`) |
| `--anchor <pat>` | Anchor windows on the first FunctionCall/CPU-profile/args match |
| `--delta` | Compare PRE/SHOOT/POST windows (frames, GC, long tasks, CPU, busy) |
| `--pre <ms>` | PRE window length before SHOOT (default 500) |
| `--post <ms>` | POST window length after SHOOT (default 500) |
| `--regex` | Treat `--events`/`--function`/`--find`/`--anchor`/`--url` as regex |
| `--json` | Emit JSON (for jq/pipelines) instead of Markdown |
| `--csv` | Emit CSV (one block per section) instead of Markdown |
| `--jank` | Jank clusters: dropped frames / spikes below the Long Task threshold |
| `--memory` | Memory timeline (JS heap, DOM nodes, documents, listeners) with peak/growth, velocity & jumps |
| `--input` | Input & media latency by type (`EventDispatch`): categories, cadence, percentiles + worst events |
| `--gaps` | Inter-event gap distribution, cadence (Hz), jitter & sequence (use with `--events`) |
| `--frame-tree` | Browser frame & document hierarchy from `TracingStartedInBrowser` (iframes, SVGs, pids) |
| `--async` | Async task timings (`s`/`f` paired by id): per-name percentiles + longest tasks |
| `--loading` | Loading milestones: FCP/LCP candidates, navigation timings, user timing |
| `--network` | Request lifecycle (Resource*/XHR): counts, cache hits, render-blocking, slowest |
| `--raster` | Compositor/raster/GPU lanes + decode hotspots (`Decode Image`, `PaintImage`) |
| `--renderers` | Per-renderer overview (pid/tid, RunTask totals, long tasks, primary marker) for multi-process traces |
| `--counters [name]` | Generic Counter (`ph C`) series: list (samples per counter) or named series table + min/max/last |
| `--flows [name]` | Async flow/causality chains (`s`/`t`/`f`/`n`, legacy `b`/`e`+id) grouped by pid+id; optional chain-name filter |
| `--junit[=FILE]` | Write a JUnit XML report (to FILE, or stdout with bare `--junit`); compare = one testcase per finding, single-trace = metric gates (see CI) |
| `--coverage` | Trust diagnostics: parsed/skipped/malformed, recognized vs unrecognized events |
| `--frame-budget <ms>` | Frame budget override (0/auto = detect refresh rate from frame cadence) |
| `--full-on-miss` | Fall back to full trace when `--anchor`/`--worst` finds nothing (default: error) |
| `--fail-on-regressed` | Exit 3 when a compare finding is `Regressed` (for CI) |
| `--sort <m>` | Sort `--events`/`--names`: `ts` (default), `dur`, `name`, `count` |
| `--out-dir <dir>` | Batch mode: write per-file `--export`/`--html`/`--json` outputs here (default: next to each trace) |
| `--tid <n\|main>` | Restrict to this thread (numeric tid or `main`) |
| `--pid <n>` | Restrict to this process |
| `--cat <substr>` | Restrict to events whose category contains this |
| `--full-args` | Print full event `args` (no truncation) for `--events`/`--find` |
| `--around <ms>` | Center of the time window (ms from trace start) |
| `--window <ms>` | Half-width of the window (default 100) |
| `--min-dur <us>` | Only events with duration ≥ this (microseconds) |
| `--top <n>` | Limit rows (default 30) |
| `--bucket <ms>` | Timeline bucket size (default auto ~40 buckets, 10–500ms); also sets the jank cluster bucket |

## Features

### Trace Health

Every full report opens with a trust block: events parsed / skipped / malformed,
main thread, trace span, detected frame budget (60/90/120Hz from frame cadence),
and throttles. `--coverage` adds the recognized-vs-unrecognized event split, so
you can tell "no problem" apart from "not detected".

### Machine-Readable Output

`--json` with no inspect query emits the full report as JSON (summary, event
stats with self-time, frame-blocking tasks, jank clusters, loading, network,
raster, compare findings). Every inspect query also supports `--json`/`--csv`.
Exit codes: `0` ok, `1` parse/IO errors (incl. per-file batch failures),
`2` usage/scope errors (anchor miss, modifiers without a query), `3` compare
regressions with `--fail-on-regressed`.

### Auto-detected Trace Metadata

Extracts from Chrome trace JSON:

- **Page URL** from `TracingStartedInBrowser` event
- **CPU Throttle** from `metadata.cpuThrottling` (e.g. 20x)
- **Record Time** from `metadata.startTime`
- **DPR** from `metadata.hostDPR`
- **Network Throttle** from `metadata.networkThrottling` (when set)

### Compare Findings

Automatically detects and flags:

- Long task count changes
- Scroll frame duration / bottleneck shifts
- JS, Style (ULT), Layout, Paint, HitTest, Composite time regressions/improvements
- Layout dirty object changes
- Style recalc element count changes (avg/max)

### Markdown Export

Exports structured Markdown (~7KB for a compare report) suitable for feeding to AI for further analysis. Includes all analysis sections, trace metadata, and throttle context.

### Summary Export (`--summary`)
PR-friendly comparison summary for use with `--compare --export --summary`. Outputs a concise, sectioned Markdown report:

| Section | Content |
|---------|---------|
| **Overall** | P50-based verdict (Improved / Regressed / No significant change) |
| **Scroll Performance** | P50, P90, avg duration + per-category breakdown (JS/Style/Layout/Paint/HitTest/Composite) |
| **Root Cause** | Style element counts (avg/max), layout dirty objects, IntersectionObserver |
| **Regressions** | Scroll-related categories that regressed >5% |
| **Notes** | Long Tasks, Main Thread Busy (marked as including non-scroll tasks), GC (MajorGC/MinorGC) |

```sh
# Output summary to stdout
chperf before.json --compare after.json --export --summary

# Save to file
chperf before.json --compare after.json --export=summary.md --summary
```

#### Example output

Ready to paste directly into a PR description:

> **Overall**: Improved (Scroll P50 -45%)
>
> | Metric | Before | After | Change |
> |--------|--------|-------|--------|
> | P50 | 370.94ms | 203.22ms | -45% :white_check_mark: |
> | P90 | 1.37s | 831.40ms | -39% :white_check_mark: |
> | Style (ULT) | 330.70ms | 174.68ms | -47% :white_check_mark: |
> | Paint | 26.61ms | 88.78ms | +234% :red_circle: |
> | HitTest | 72.28ms | 28.49ms | -61% :white_check_mark: |

### JUnit XML for CI (`--junit`)

```sh
# Single trace: metric gates → report.xml (report still prints to stdout)
chperf trace.json --junit=report.xml

# Bare --junit: XML only on stdout (alongside nothing else)
chperf trace.json --junit | xunit-viewer

# Compare: one testcase per finding (Regressed = failure)
chperf before.json --compare after.json --junit=report.xml
```

Report-only: `--junit` never changes the exit code (exit 3 still comes
only from `--fail-on-regressed`). `--junit` with `--compare` needs the full
report path (no inspect flags); bare `--junit` cannot share stdout with an
inspect query (use `--junit=FILE` there).

| Mode | Testcase | Failure when |
|------|----------|--------------|
| compare | `{category}: {message}` (classname `chperf.compare`) | Finding severity is `Regressed` (Improved/Neutral pass; detail = finding detail) |
| single: `long-tasks` | `{N} long tasks (>50ms)` | Any main-thread RunTask > 50ms |
| single: `dropped-frames` | `{N} dropped frames` | Any `DroppedFrame` recorded |
| single: `total-blocking-time` | `TBT {ms} exceeds 500ms` | Sum of long-task time past 50ms > 500ms |
| single: `frame-blocking-tasks` | `{N} frame-blocking tasks` | Any attributed frame-blocking main-thread task (scroll or heuristic fallback) |

## Analyzed Events

`RunTask`, `UpdateLayoutTree`, `Layout`, `Paint`, `FunctionCall`, `FireAnimationFrame`, `Layerize`, `Commit`, `HitTest`, `IntersectionObserverController::computeIntersections`, `MajorGC`, `MinorGC`, `EvaluateScript`
