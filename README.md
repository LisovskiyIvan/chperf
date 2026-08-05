# chperf

Chrome DevTools Performance trace analyzer with TUI. Parses `{ "traceEvents": [...] }` JSON and surfaces actionable performance insights.

Accepts plain `.json` and gzipped `.json.gz` traces — a single file, or a directory (list / batch export).

Chrome DevTools trace JSON files are massive and impractical to read by hand. chperf structures and summarizes the trace, then exports it as Markdown. Feed the exported result directly to an LLM like Claude Code for instant bottleneck identification and improvement suggestions.

![TUI Screenshot](capture.png)

## Install

```sh
cargo install --path .
```

Requires Rust 2024 edition (1.85+).

## Usage

```sh
# Single trace analysis (TUI)
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
```

## REPL (interactive)

Load and analyze a trace **once**, then query it live — every command is a
single pass over the in-memory data, no re-parse:

```sh
chperf --repl trace.json.gz
```

```sh
> names --top 5
> events RunTask --sort dur --top 10 --around 42555600 --window 1000
> function render --top 10
> find setPlayerRespawned --regex
> worst --task
> compare after.json.gz        # load second trace, rebuild compare
> export report.md             # full markdown export of current app
> summary                      # compare summary (after `compare`)
> throttle 6 | status | clear | help | quit
```

Commands mirror the CLI flags (bare words map to flags: `events X` ≡
`--events X`, `export=file` ≡ `--export=file`); `--around/--window` anchor
subsequent time-windowed queries. Data is loaded once — a 1.3M-event trace
loads in ~1.5s and every query answers in tens of milliseconds.

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
| `--sort <m>` | Sort `--events`/`--names`: `ts` (default), `dur`, `name`, `count` |
| `--tid <n\|main>` | Restrict to this thread (numeric tid or `main`) |
| `--pid <n>` | Restrict to this process |
| `--cat <substr>` | Restrict to events whose category contains this |
| `--full-args` | Print full event `args` (no truncation) for `--events`/`--find` |
| `--around <ms>` | Center of the time window (ms from trace start) |
| `--window <ms>` | Half-width of the window (default 100) |
| `--min-dur <us>` | Only events with duration ≥ this (microseconds) |
| `--top <n>` | Limit rows (default 30) |
| `--bucket <ms>` | Timeline bucket size (default auto ~40 buckets, 10–500ms) |

## Features

### Tabs

| Tab | What it shows |
|-----|---------------|
| **Summary** | Trace metadata, main thread busy %, long tasks (>50ms) with histogram, event breakdown table, forced reflow detection, style recalc element counts |
| **Scroll Frames** | Scroll tasks (RunTask containing ULT>50ms or FunctionCall>50ms), avg/P50/P90/P99 duration, bottleneck analysis, per-task breakdown bars (JS/Style/Layout/Paint/Composite/HitTest/Other) |
| **CPU Profile** | Top functions by self-time from ProfileChunk events, source classification (App/Runtime/Native), stacked distribution bar |
| **Layout Dirty** | Layout events with dirty/total object counts, avg dirty ratio |
| **Jank** | Jank clusters: windows with dropped frames or ≥16.7ms spikes (RunTask/FireAnimationFrame/GPUTask) that stay below the 50ms Long Task threshold and disappear in a long trace's average — with the dominating function chain ("what happened") |
| **Compare** | Side-by-side scroll breakdown bars, key findings (auto-detected regressions/improvements), quick stats diff, style element count comparison, event average diff, CPU profile diff by percentage-point impact |

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

## TUI Keybindings

| Key | Action |
|-----|--------|
| `Tab` / `Shift+Tab` | Next / previous tab |
| `1`-`5` | Jump to tab directly |
| `j` / `k` / `Up` / `Down` | Scroll |
| `Ctrl+d` / `Ctrl+u` | Page down / up |
| `g` / `G` | Top / bottom |
| `t` | Toggle CPU throttle display (trace time vs real time) |
| `e` | Export current analysis to `chperf-export-<name>.md` |
| `q` / `Esc` / `Ctrl+c` | Quit |

## Analyzed Events

`RunTask`, `UpdateLayoutTree`, `Layout`, `Paint`, `FunctionCall`, `FireAnimationFrame`, `Layerize`, `Commit`, `HitTest`, `IntersectionObserverController::computeIntersections`, `MajorGC`, `MinorGC`, `EvaluateScript`
