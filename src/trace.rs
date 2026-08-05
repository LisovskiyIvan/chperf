use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Deserialize, Clone)]
pub struct TraceMetadata {
    #[serde(rename = "cpuThrottling", default)]
    pub cpu_throttling: Option<f64>,
    #[allow(dead_code)]
    #[serde(default)]
    pub source: Option<String>,
    #[serde(rename = "startTime", default)]
    pub start_time: Option<String>,
    #[serde(rename = "networkThrottling", default)]
    pub network_throttling: Option<String>,
    #[serde(rename = "hardwareConcurrency", default)]
    pub hardware_concurrency: Option<u32>,
    #[serde(rename = "hostDPR", default)]
    pub host_dpr: Option<f64>,
    /// Extracted from TracingStartedInBrowser (not in JSON metadata)
    #[serde(skip)]
    pub page_url: Option<String>,
}

#[derive(Deserialize)]
pub struct TraceFile {
    #[serde(rename = "traceEvents")]
    pub trace_events: Vec<TraceEvent>,
    #[serde(default)]
    pub metadata: Option<TraceMetadata>,
}

#[derive(Deserialize)]
pub struct TraceEvent {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ph: String,
    #[serde(default)]
    pub ts: f64,
    #[serde(default)]
    pub dur: Option<f64>,
    #[serde(default)]
    pub tid: u64,
    #[serde(default)]
    #[allow(dead_code)]
    pub pid: u64,
    #[serde(default)]
    #[allow(dead_code)]
    pub cat: Option<String>,
    /// Raw `args` JSON, captured without building a Value tree. Most events
    /// never have their args inspected, so storing the raw bytes keeps the
    /// load fast and memory low; `args_value()` parses on demand (and caches
    /// the result, so repeated scans of ProfileChunk payloads stay cheap).
    #[serde(default)]
    pub args: Option<Box<serde_json::value::RawValue>>,
    #[serde(skip)]
    pub(crate) args_cache: std::sync::OnceLock<Option<serde_json::Value>>,
}

/// Test helper: build an event `args` field from a JSON value (stored raw).
#[cfg(test)]
pub(crate) fn test_args(v: serde_json::Value) -> Option<Box<serde_json::value::RawValue>> {
    serde_json::to_string(&v)
        .ok()
        .map(|s| serde_json::from_str(&s).expect("raw args json"))
}

impl Clone for TraceEvent {
    fn clone(&self) -> Self {
        TraceEvent {
            name: self.name.clone(),
            ph: self.ph.clone(),
            ts: self.ts,
            dur: self.dur,
            tid: self.tid,
            pid: self.pid,
            cat: self.cat.clone(),
            args: self.args.clone(),
            args_cache: std::sync::OnceLock::new(),
        }
    }
}

impl TraceEvent {
    /// Parsed `args` JSON, or `None` when the event carries no args. The
    /// first call per event parses and caches the raw bytes.
    pub fn args_value(&self) -> Option<&serde_json::Value> {
        self.args_cache
            .get_or_init(|| {
                self.args
                    .as_deref()
                    .and_then(|r| serde_json::from_str(r.get()).ok())
            })
            .as_ref()
    }

    /// Raw `args` JSON text, or `None`.
    #[allow(dead_code)]
    pub fn args_raw(&self) -> Option<&str> {
        self.args.as_deref().map(|r| r.get())
    }
}

/// Byte ranges of the top-level `traceEvents` array and the `metadata`
/// object, located by a block-counting scan.
struct Layout {
    arr_open: usize,
    arr_close: usize,
    metadata: Option<(usize, usize)>,
}

/// Per-64KB-block JSON structure: brace depth and in-string state at each
/// block start. Depth is computed from raw brace/bracket counts (ignoring
/// strings — validated to match the exact state on real traces; strings in
/// Chrome traces almost never contain braces), in-string state from exact
/// (escape-aware) quote parity. Both are cheap SIMD-counted, so the full
/// element walk only runs inside the parse chunks.
struct Blocks {
    depth_at: Vec<i64>,
    in_string_at: Vec<bool>,
}

const BLOCK: usize = 64 * 1024;

fn build_blocks(bytes: &[u8]) -> Blocks {
    let nb = bytes.len().div_ceil(BLOCK);
    let threads = std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(1)
        .max(1);
    let chunk_count = if threads >= 4 && nb >= 64 { threads } else { 1 };

    // Per-block counts are independent — count in parallel.
    let mut opens = vec![0i64; nb];
    let mut closes = vec![0i64; nb];
    let mut quote_parity = vec![false; nb];
    if chunk_count > 1 {
        let ptr_o: usize = opens.as_mut_ptr() as usize;
        let ptr_c: usize = closes.as_mut_ptr() as usize;
        let ptr_q: usize = quote_parity.as_mut_ptr() as usize;
        std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(chunk_count);
            for t in 0..chunk_count {
                let lo = nb * t / chunk_count;
                let hi = nb * (t + 1) / chunk_count;
                let bstart = lo * BLOCK;
                let bend = (hi * BLOCK).min(bytes.len());
                let bytes = &bytes[bstart..bend];
                handles.push(s.spawn(move || {
                    let ptr_o = ptr_o as *mut i64;
                    let ptr_c = ptr_c as *mut i64;
                    let ptr_q = ptr_q as *mut bool;
                    // Exact unescaped-quote parity per block: a quote is
                    // escaped iff preceded by an odd run of backslashes.
                    let mut prev_trail = 0usize; // backslashes ending previous block
                    for (k, chunk) in bytes.chunks(BLOCK).enumerate() {
                        let bi = lo + k;
                        unsafe {
                            *ptr_o.add(bi) = memchr::memchr2_iter(b'{', b'[', chunk).count() as i64;
                            *ptr_c.add(bi) = memchr::memchr2_iter(b'}', b']', chunk).count() as i64;
                        }
                        let mut nq = 0usize;
                        for q in memchr::memchr_iter(b'"', chunk) {
                            let mut run = 0usize;
                            let mut kk = q;
                            while kk > 0 && chunk[kk - 1] == b'\\' {
                                run += 1;
                                kk -= 1;
                            }
                            let escaped = if kk == 0 {
                                (run + prev_trail) % 2 == 1
                            } else {
                                run % 2 == 1
                            };
                            if !escaped {
                                nq += 1;
                            }
                        }
                        unsafe { *ptr_q.add(bi) = nq % 2 == 1; }
                        let mut t2 = 0usize;
                        let mut kk = chunk.len();
                        while kk > 0 && chunk[kk - 1] == b'\\' {
                            t2 += 1;
                            kk -= 1;
                        }
                        prev_trail = t2;
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }
        });
    } else {
        for (k, chunk) in bytes.chunks(BLOCK).enumerate() {
            opens[k] = memchr::memchr2_iter(b'{', b'[', chunk).count() as i64;
            closes[k] = memchr::memchr2_iter(b'}', b']', chunk).count() as i64;
            let mut nq = 0usize;
            for q in memchr::memchr_iter(b'"', chunk) {
                let mut run = 0usize;
                let mut kk = q;
                while kk > 0 && chunk[kk - 1] == b'\\' {
                    run += 1;
                    kk -= 1;
                }
                if run.is_multiple_of(2) {
                    nq += 1;
                }
            }
            quote_parity[k] = nq % 2 == 1;
        }
    }

    // Sequential prefix sums: depth and in-string state at each block start.
    let mut depth_at = Vec::with_capacity(nb + 1);
    let mut in_string_at = Vec::with_capacity(nb + 1);
    let mut d = 0i64;
    let mut s = false;
    depth_at.push(0);
    in_string_at.push(false);
    for k in 0..nb {
        d += opens[k] - closes[k];
        s ^= quote_parity[k];
        depth_at.push(d);
        in_string_at.push(s);
    }
    Blocks {
        depth_at,
        in_string_at,
    }
}

/// Exact (string-aware) depth and in-string state at byte `pos`, by walking
/// only the (≤64KB) block prefix. `d`/`s` are the block-start state.
fn state_at(bytes: &[u8], blocks: &Blocks, pos: usize) -> (i64, bool) {
    let block = pos / BLOCK;
    let mut d = blocks.depth_at[block];
    let mut s = blocks.in_string_at[block];
    walk_range(&bytes[block * BLOCK..pos], &mut d, &mut s);
    (d, s)
}

/// String-aware walk over a byte range, updating depth and in-string state.
fn walk_range(r: &[u8], d: &mut i64, s: &mut bool) {
    let mut in_str = *s;
    let mut esc = false;
    for &b in r {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' | b'[' => *d += 1,
                b'}' | b']' => *d -= 1,
                _ => {}
            }
        }
    }
    *s = in_str;
}

/// Find the `[` (traceEvents) or `{` (metadata) opening the value of a
/// top-level member key, at depth 1. Uses SIMD substring search + local
/// probes; returns (position, byte-of-opener).
fn find_top_level_open(bytes: &[u8], blocks: &Blocks, needle: &[u8]) -> Option<(usize, u8)> {
    for m in memchr::memmem::find_iter(bytes, needle) {
        if m == 0 || bytes[m - 1] != b'"' {
            continue;
        }
        let after = m + needle.len();
        if after >= bytes.len() || bytes[after] != b'"' {
            continue;
        }
        let mut j = after + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() {
            continue;
        }
        let opener = bytes[j];
        if opener != b'[' && opener != b'{' {
            continue;
        }
        let (d, _) = state_at(bytes, blocks, j);
        if d == 1 {
            return Some((j, opener));
        }
    }
    None
}

/// Block-accelerated layout scan: array bounds via targeted probes, array
/// close via the last block at array depth, metadata via key probe.
/// Falls back to `None` when the structure isn't recognized.
fn scan_layout(bytes: &[u8]) -> Option<(Layout, Blocks)> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let blocks = build_blocks(bytes);
    let (arr_open, _opener) = find_top_level_open(bytes, &blocks, b"traceEvents")?;
    if debug {
        eprintln!("  [scan] arr_open at {}", arr_open);
    }
    if arr_open + 1 >= bytes.len() {
        return None;
    }
    // Array close `]`: walk blocks from the end of the file backwards. The
    // block containing the `]` starts somewhere inside the last element
    // (depth ≥ 2) and the walk reaches depth 2 exactly at the array close.
    let nb = blocks.depth_at.len() - 1;
    let mut arr_close = None;
    for k in (0..nb).rev().take(16) {
        let bs = k * BLOCK;
        let be = (bs + BLOCK).min(bytes.len());
        let mut d = blocks.depth_at[k];
        let mut s = blocks.in_string_at[k];
        let mut i = bs;
        while i < be {
            let b = bytes[i];
            if s {
                if b == b'"' {
                    let mut run = 0usize;
                    let mut kk = i;
                    while kk > bs && bytes[kk - 1] == b'\\' {
                        run += 1;
                        kk -= 1;
                    }
                    if run.is_multiple_of(2) {
                        s = false;
                    }
                }
            } else {
                match b {
                    b'"' => s = true,
                    b'{' | b'[' => d += 1,
                    b'}' | b']' => {
                        if b == b']' && d == 2 {
                            arr_close = Some(i);
                            break;
                        }
                        d -= 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        if arr_close.is_some() {
            break;
        }
    }
    if debug {
        eprintln!("  [scan] blocks={}, arr_close={:?}", nb, arr_close);
    }
    let arr_close = arr_close?;
    if arr_close <= arr_open {
        return None;
    }

    let metadata = find_top_level_open(bytes, &blocks, b"metadata").and_then(|(pos, opener)| {
        if opener != b'{' {
            return None;
        }
        // Closing brace of the metadata object: walk from its block.
        let block = pos / BLOCK;
        let bs = block * BLOCK;
        let mut d = blocks.depth_at[block];
        let mut s = blocks.in_string_at[block];
        let mut end = None;
        for (i, &b) in bytes[bs..].iter().enumerate() {
            let idx = bs + i;
            if s {
                if b == b'"' {
                    let mut run = 0usize;
                    let mut k = idx;
                    while k > bs && bytes[k - 1] == b'\\' {
                        run += 1;
                        k -= 1;
                    }
                    if run.is_multiple_of(2) {
                        s = false;
                    }
                }
            } else {
                match b {
                    b'"' => s = true,
                    b'{' | b'[' => d += 1,
                    b'}' | b']' => {
                        if b == b'}' && d == 2 {
                            end = Some(idx + 1);
                            break;
                        }
                        d -= 1;
                    }
                    _ => {}
                }
            }
        }
        end.map(|e| (pos, e))
    });

    Some((
        Layout {
            arr_open,
            arr_close,
            metadata,
        },
        blocks,
    ))
}

fn parse_thread_count(n: &usize) -> usize {
    std::env::var("CHPERF_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|t| t.get())
                .unwrap_or(1)
        })
        .min(*n)
        .max(1)
}

/// One thread's parse result: events plus the separator-comma positions it
/// replaced (for restore on failure).
type ChunkResult = Result<(Vec<TraceEvent>, Vec<usize>), String>;

/// Parse the `traceEvents` array in parallel. Each thread walks its own byte
/// range (starting from a block boundary with known exact state), finds the
/// element ranges, replaces their separator commas with spaces in-place
/// (StreamDeserializer rejects commas), then parses the slice directly from
/// the original buffer — zero copying. On failure all commas are restored
/// and the caller falls back to a whole-buffer parse.
fn parse_parallel(bytes: &mut [u8], layout: &Layout, blocks: &Blocks) -> Result<(Vec<TraceEvent>, Option<TraceMetadata>), Box<dyn std::error::Error>> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let threads = parse_thread_count(&(layout.arr_close - layout.arr_open));
    let span = layout.arr_close - layout.arr_open;

    let t_scope = std::time::Instant::now();

    // Per-thread: (events, comma positions for restore).
    let results: Vec<ChunkResult> = if threads <= 1 {
        let r = chunk_work(bytes, blocks, layout.arr_open, layout.arr_close);
        vec![r]
    } else {
        std::thread::scope(|s| {
            let ptr: usize = bytes.as_ptr() as usize;
            let mut handles = Vec::with_capacity(threads);
            for t in 0..threads {
                let from = layout.arr_open + span * t / threads;
                let to = if t + 1 == threads {
                    layout.arr_close
                } else {
                    layout.arr_open + span * (t + 1) / threads
                };
                handles.push(s.spawn(move || {
                    let t0 = std::time::Instant::now();
                    let bytes = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, layout.arr_close) };
                    let r = chunk_work(bytes, blocks, from, to);
                    if debug {
                        eprintln!(
                            "  [parse] chunk {}..{}: {:.1}ms, {} events",
                            from,
                            to,
                            t0.elapsed().as_secs_f64() * 1000.0,
                            r.as_ref().map(|(v, _)| v.len()).unwrap_or(0)
                        );
                    }
                    r
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("chunk thread panicked".into()))).collect()
        })
    };

    // Unwrap chunk results; on any failure restore commas and propagate.
    let mut parsed: Vec<Vec<TraceEvent>> = Vec::with_capacity(threads);
    let mut replaced: Vec<usize> = Vec::new();
    for r in results {
        match r {
            Ok((events, commas)) => {
                parsed.push(events);
                replaced.extend(commas);
            }
            Err(e) => {
                for j in replaced {
                    bytes[j] = b',';
                }
                return Err(Box::<dyn std::error::Error>::from(format!("chunk parse failed: {}", e)));
            }
        }
    }
    if debug {
        eprintln!(
            "  [parse] scope: {:.1}ms",
            t_scope.elapsed().as_secs_f64() * 1000.0
        );
    }

    let t_merge = std::time::Instant::now();
    let total: usize = parsed.iter().map(|p| p.len()).sum();
    // Move (not clone!) chunk elements into one contiguous Vec. `append`
    // transfers ownership with a shallow memcpy and never drops the source
    // elements — deep-cloning 1.35M events would be ~10x slower.
    let mut events: Vec<TraceEvent> = Vec::with_capacity(total);
    for mut p in parsed {
        events.append(&mut p);
    }
    if debug {
        eprintln!("  [parse] merge: {:.1}ms", t_merge.elapsed().as_secs_f64() * 1000.0);
    }

    let metadata = match layout.metadata {
        Some((s, e)) => serde_json::from_slice::<TraceMetadata>(&bytes[s..e]).ok(),
        None => None,
    };
    Ok((events, metadata))
}

/// Walk `[from_block_start, to)` (extended to finish the element containing
/// `to`), collecting element ranges starting ≥ `from`, mutating their
/// separator commas, and parsing the slice. Returns the events and the
/// mutated comma positions. The walk visits only structural positions
/// (memchr-accelerated), not every byte.
fn chunk_work(
    bytes: &mut [u8],
    blocks: &Blocks,
    from: usize,
    to: usize,
) -> Result<(Vec<TraceEvent>, Vec<usize>), String> {
    let block = from / BLOCK;
    let start = block * BLOCK;
    let mut d = blocks.depth_at[block];
    let mut in_string = blocks.in_string_at[block];
    let mut replaced: Vec<usize> = Vec::new();

    let mut first_start: Option<usize> = None;
    let mut last_end: usize = 0;
    let mut elem_start: Option<usize> = None;
    let mut finished_last = false; // the element containing `to` was completed

    // Walk to the end of the chunk (and finish the element containing `to`).
    let end = if to + BLOCK > bytes.len() {
        bytes.len()
    } else {
        to + BLOCK
    };
    let mut strings = memchr::memchr3_iter(b'"', b'{', b'[', &bytes[start..end]);
    let mut closes = memchr::memchr2_iter(b'}', b']', &bytes[start..end]);
    let mut q = strings.next();
    let mut c = closes.next();
    loop {
        let (rel, b) = match (q, c) {
            (Some(p), _) if c.is_none_or(|x| p < x) => {
                q = strings.next();
                (p, bytes[start + p])
            }
            (_, Some(p)) => {
                c = closes.next();
                (p, bytes[start + p])
            }
            _ => break,
        };
        let i = start + rel;
        if in_string {
            if b == b'"' {
                let mut run = 0usize;
                let mut k = i;
                while k > start && bytes[k - 1] == b'\\' {
                    run += 1;
                    k -= 1;
                }
                if run.is_multiple_of(2) {
                    in_string = false;
                }
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => {
                if b == b'{' && d == 2 && i >= from {
                    elem_start = Some(i);
                }
                d += 1;
            }
            b'}' | b']' => {
                if b == b'}' && d == 3 {
                    if let Some(e) = elem_start.take() {
                        // A chunk owns elements whose start is inside
                        // [from, to). An element starting at/after `to`
                        // belongs to the next chunk — stopping here keeps
                        // its separator comma outside this slice.
                        if e >= to {
                            break;
                        }
                        if first_start.is_none() {
                            first_start = Some(e);
                        }
                        last_end = i + 1;
                        // Record the separator comma (skip whitespace);
                        // mutation happens after the walk (the memchr
                        // iterators hold an immutable borrow).
                        let mut j = i + 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b',' && j < to {
                            replaced.push(j);
                        }
                        // Stop at the first element boundary at/after `to`
                        // even when the element *containing* `to` started
                        // before `from` (it belongs to the previous chunk):
                        // otherwise the walk grabs the next element, which
                        // the following chunk also owns — boundary elements
                        // get parsed twice.
                        if i >= to {
                            finished_last = true;
                        }
                    } else if i >= to {
                        // Element started before `from` (previous chunk's)
                        // but spans at least to `to`: stop here.
                        finished_last = true;
                    }
                }
                d -= 1;
            }
            _ => {}
        }
        if finished_last {
            break;
        }
    }

    // Separator commas → spaces, in place (disjoint per chunk).
    for j in &replaced {
        bytes[*j] = b' ';
    }

    let (s, e) = match (first_start, last_end) {
        (Some(s), e) if e > s => (s, e),
        _ => return Ok((Vec::new(), replaced)),
    };

    // Separator commas → spaces, in place (disjoint per chunk).
    for j in &replaced {
        bytes[*j] = b' ';
    }

    let events = serde_json::Deserializer::from_slice(&bytes[s..e])
        .into_iter()
        .collect::<Result<Vec<TraceEvent>, _>>()
        .map_err(|err| {
            if std::env::var("CHPERF_DEBUG").is_ok() {
                eprintln!("  [dbg] slice [{s}..{e}): {}", String::from_utf8_lossy(&bytes[s..e]));
            }
            format!("{}", err)
        })?;
    Ok((events, replaced))
}


pub fn parse_trace(path: &Path) -> Result<TraceFile, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    // Decompress (if .gz) and parse from an in-memory slice. This is far
    // faster than streaming serde_json through the gzip decoder: zlib-rs
    // inflates at ~1.5GB/s, and `from_slice` skips reader indirection.
    let mut bytes = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(file).read_to_end(&mut out)?;
        out
    } else {
        let mut out = Vec::with_capacity(file.metadata().map(|m| m.len() as usize).unwrap_or(0));
        file.take(u64::MAX).read_to_end(&mut out)?;
        out
    };

    // Fast path: chunk-parallel parse of the traceEvents array.
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let mut trace: Option<TraceFile> = None;
    let t_scan = std::time::Instant::now();
    if let Some((layout, blocks)) = scan_layout(&bytes) {
        if debug {
            eprintln!(
                "  [parse] scan {:.1}ms: array {}..{}, metadata={}",
                t_scan.elapsed().as_secs_f64() * 1000.0,
                layout.arr_open,
                layout.arr_close,
                layout.metadata.is_some()
            );
        }
        let t_par = std::time::Instant::now();
        if debug {
            eprintln!(
                "  [parse] threads: {}",
                parse_thread_count(&(layout.arr_close - layout.arr_open))
            );
        }
        match parse_parallel(&mut bytes, &layout, &blocks) {
            Ok((trace_events, metadata)) => {
                if debug {
                    eprintln!(
                        "  [parse] parallel {:.1}ms: {} events",
                        t_par.elapsed().as_secs_f64() * 1000.0,
                        trace_events.len()
                    );
                }
                trace = Some(TraceFile {
                    trace_events,
                    metadata,
                });
            }
            Err(e) if debug => eprintln!("  [parse] parallel failed: {}", e),
            _ => {}
        }
    } else if debug {
        eprintln!("  [parse] scan returned None, falling back");
    }
    // Fallback: whole-buffer parse (unusual layout, non-object elements, …).
    // Note: must NOT use unwrap_or — its argument is evaluated eagerly and
    // would re-parse the comma-mutated buffer on the happy path.
    let mut trace = match trace {
        Some(t) => t,
        None => serde_json::from_slice(&bytes)?,
    };
    if debug {
        eprintln!("  [parse] trace ready: {} events", trace.trace_events.len());
    }

    // Extract page URL from TracingStartedInBrowser event
    {
        let meta = trace.metadata.get_or_insert(TraceMetadata {
            cpu_throttling: None,
            source: None,
            start_time: None,
            network_throttling: None,
            hardware_concurrency: None,
            host_dpr: None,
            page_url: None,
        });
        if meta.page_url.is_none() {
            for e in &trace.trace_events {
                if e.name == "TracingStartedInBrowser" {
                    if let Some(args) = e.args_value()
                        && let Some(frames) = args
                            .get("data")
                            .and_then(|d| d.get("frames"))
                            .and_then(|f| f.as_array())
                        {
                            for frame in frames {
                                if let Some(url) = frame.get("url").and_then(|u| u.as_str())
                                    && !url.is_empty() && url != "about:blank" {
                                        meta.page_url = Some(url.to_string());
                                        break;
                                    }
                            }
                        }
                    break;
                }
            }
        }
    }

    Ok(trace)
}

/// Metadata events (`thread_name`/`process_name`/…, cat `__metadata`) carry
/// `ts` from process start — often far before the actual session. They must be
/// excluded from time-base computations, or every "ms from trace start" window
/// lands in dead time.
pub fn is_metadata_event(e: &TraceEvent) -> bool {
    matches!(
        e.name.as_str(),
        "thread_name" | "process_name" | "thread_sort_index" | "process_sort_index"
    ) || e.cat.as_deref() == Some("__metadata")
}

/// Detect main thread: first RunTask with dur > 500ms
pub fn detect_main_thread(events: &[TraceEvent]) -> u64 {
    for e in events {
        if e.name == "RunTask" && e.ph == "X"
            && let Some(dur) = e.dur
                && dur > 500_000.0 {
                    return e.tid;
                }
    }
    // Fallback: tid with most RunTask events
    let mut counts: rustc_hash::FxHashMap<u64, usize> = rustc_hash::FxHashMap::default();
    for e in events {
        if e.name == "RunTask" && e.ph == "X" {
            *counts.entry(e.tid).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(tid, _)| tid)
        .unwrap_or(0)
}

/// Stable stem for a trace file: strips both `.json` and `.json.gz`.
/// `Trace-20260731T180758.json.gz` -> `Trace-20260731T180758`.
pub fn trace_stem(path: &Path) -> String {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let name = name.strip_suffix(".json").unwrap_or(name);
    name.to_string()
}

/// Scan a directory for Chrome trace files (`*.json`, `*.json.gz`).
/// When both `.json` and `.json.gz` exist for the same stem, keep only the
/// `.json.gz` (smaller I/O). Sorted by name.
pub fn list_traces(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut by_stem: std::collections::BTreeMap<String, PathBuf> = std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        let is_gz = name.ends_with(".json.gz");
        let is_plain = !is_gz && name.ends_with(".json");
        if !is_gz && !is_plain {
            continue;
        }
        let stem = trace_stem(&path);
        match by_stem.get(&stem) {
            // Prefer .gz: replace a plain entry when we meet its gz twin.
            Some(existing) if existing.extension().and_then(|e| e.to_str()) == Some("gz") => {}
            _ => {
                by_stem.insert(stem, path);
            }
        }
    }
    Ok(by_stem.into_values().collect())
}
