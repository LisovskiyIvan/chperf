//! Command-line arguments (clap). Pure arg parsing, no trace/analysis deps.

use clap::Parser;

#[derive(Parser)]
#[command(name = "chperf", about = "Chrome DevTools Trace JSON analyzer (CLI)")]
pub(crate) struct Cli {
    /// Path to trace JSON file (.json or .json.gz) or a directory of traces
    pub(crate) trace: Option<String>,

    /// Interactive REPL: load and analyze once, then query live
    #[arg(long)]
    pub(crate) repl: bool,

    /// Optional second trace file for comparison
    #[arg(short, long)]
    pub(crate) compare: Option<String>,

    /// Export analysis as Markdown (to stdout or file)
    /// Use --export to print to stdout, --export=FILE to write to file
    #[arg(short, long, num_args = 0..=1, default_missing_value = "-")]
    pub(crate) export: Option<String>,

    /// Export a self-contained HTML report (single file, SVG charts)
    /// Use --html to print to stdout, --html=FILE to write to file
    #[arg(long, num_args = 0..=1, default_missing_value = "-")]
    pub(crate) html: Option<String>,

    /// CPU throttle factor (e.g. --throttle 20 divides all times by 20)
    #[arg(short, long)]
    pub(crate) throttle: Option<f64>,

    /// Export only the comparison summary table (use with --export --compare)
    #[arg(short, long)]
    pub(crate) summary: bool,

    /// Inspect: list events by name (comma-separated), e.g. --events GPUTask,RunTask
    #[arg(long)]
    pub(crate) events: Option<String>,

    /// Inspect: aggregate CPU samples whose function name contains this substring
    #[arg(long)]
    pub(crate) function: Option<String>,

    /// Inspect: search event args (JSON) for this substring
    #[arg(long)]
    pub(crate) find: Option<String>,

    /// Inspect: center of the time window, in ms from trace start (use with --window)
    #[arg(long)]
    pub(crate) around: Option<f64>,

    /// Inspect: half-width of the time window in ms (default 100, use with --around)
    #[arg(long)]
    pub(crate) window: Option<f64>,

    /// Inspect: only events with duration >= this value, in microseconds
    #[arg(long)]
    pub(crate) min_dur: Option<f64>,

    /// Inspect: limit number of rows (default 30)
    #[arg(long, default_value_t = 30)]
    pub(crate) top: usize,

    /// Inspect: restrict events/functions/find to this thread (numeric tid or "main")
    #[arg(long)]
    pub(crate) tid: Option<String>,

    /// Inspect: restrict events/functions/find to this process id (pid)
    #[arg(long)]
    pub(crate) pid: Option<u64>,

    /// Inspect: restrict to events whose category (cat) contains this substring
    #[arg(long)]
    pub(crate) cat: Option<String>,

    /// Inspect: list distinct event names with counts/total duration
    #[arg(long)]
    pub(crate) names: bool,

    /// Inspect: list distinct threads (tid) with counts/RunTask duration
    #[arg(long)]
    pub(crate) threads: bool,

    /// Inspect: heaviest CPU call stacks (root → leaf), heaviest first
    #[arg(long)]
    pub(crate) stacks: bool,

    /// Inspect: folded stacks (`a;b;c <us>`) for flamegraph.pl / speedscope
    #[arg(long)]
    pub(crate) flame: bool,

    /// Inspect: break down the heaviest RunTasks into their child events
    #[arg(long)]
    pub(crate) task: bool,

    /// Inspect: busy timeline (RunTask per time bucket)
    #[arg(long)]
    pub(crate) timeline: bool,

    /// Inspect: timeline bucket size in ms (default auto ~40 buckets, 10-500ms)
    #[arg(long)]
    pub(crate) bucket: Option<f64>,

    /// Inspect: duration distribution table instead of event list (use with --events)
    #[arg(long)]
    pub(crate) stats: bool,

    /// Inspect: auto-anchor --around on the worst (longest) RunTask
    #[arg(long)]
    pub(crate) worst: bool,

    /// Inspect: sort order for --events/--names (ts, dur, name, count)
    #[arg(long)]
    pub(crate) sort: Option<String>,

    /// Inspect: print full event args (no truncation) for --events/--find
    #[arg(long)]
    pub(crate) full_args: bool,

    /// Inspect: interpret --function/--find/--events as regex instead of substring/exact
    #[arg(long)]
    pub(crate) regex: bool,

    /// Inspect: emit JSON (for jq/pipelines) instead of Markdown
    #[arg(long)]
    pub(crate) json: bool,

    /// Inspect: emit CSV instead of Markdown (--json for JSON)
    #[arg(long)]
    pub(crate) csv: bool,

    /// Inspect: jank clusters (dropped frames / spikes below Long Task threshold)
    #[arg(long)]
    pub(crate) jank: bool,

    /// Inspect: memory timeline (UpdateCounters: JS heap, DOM nodes, documents,
    /// event listeners) with peak / growth summary
    #[arg(long)]
    pub(crate) memory: bool,

    /// Inspect: input latency by type (EventDispatch): percentiles + worst events
    #[arg(long)]
    pub(crate) input: bool,

    /// Inspect: async task timings (s/f events paired by id): per-name
    /// percentiles + longest tasks
    #[arg(long = "async")]
    pub(crate) async_: bool,

    /// Inspect: anchor windows on the first FunctionCall functionName /
    /// CPU profile function or URL / event-args match of this substring
    #[arg(long)]
    pub(crate) anchor: Option<String>,

    /// Inspect: PRE window length in ms before the SHOOT window (default 500)
    #[arg(long, default_value_t = 500.0)]
    pub(crate) pre: f64,

    /// Inspect: POST window length in ms after the SHOOT window (default 500)
    #[arg(long, default_value_t = 500.0)]
    pub(crate) post: f64,

    /// Inspect: compare PRE / SHOOT / POST windows (frames, dropped frames,
    /// GC, long tasks, CPU samples, busy time) with deltas
    #[arg(long)]
    pub(crate) delta: bool,

    /// Inspect: inclusive CPU call tree (self + subtree time); prune with
    /// --function / --url
    #[arg(long)]
    pub(crate) calltree: bool,

    /// Inspect: restrict CPU functions/stacks/calltree to source URLs
    /// containing this substring
    #[arg(long)]
    pub(crate) url: Option<String>,

    /// Inspect: GC + long-task report for the window
    #[arg(long)]
    pub(crate) gc: bool,

    /// Inspect: long-task threshold in ms (default 50, use with --gc/--delta)
    #[arg(long, default_value_t = 50.0)]
    pub(crate) lt: f64,

    /// Inspect: per-frame duration stats (b/e-paired events) for the window
    #[arg(long)]
    pub(crate) frames: bool,

    /// Inspect: frame event name for --frames/--delta
    #[arg(long, default_value = "SubmitCompositorFrameToPresentationCompositorFrame")]
    pub(crate) frame_event: String,
}

impl Cli {
    /// Any granular-inspect flag present?
    pub(crate) fn is_inspect(&self) -> bool {
        self.events.is_some()
            || self.function.is_some()
            || self.find.is_some()
            || self.names
            || self.threads
            || self.stacks
            || self.flame
            || self.task
            || self.timeline
            || self.worst
            || self.jank
            || self.memory
            || self.input
            || self.async_
            || self.anchor.is_some()
            || self.delta
            || self.calltree
            || self.gc
            || self.frames
    }
}
