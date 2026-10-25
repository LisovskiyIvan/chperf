//! Streaming, windowed parse for large trace files.
//!
//! The whole-file path keeps the entire inflated text in RAM for the whole
//! run (every event's `args` is a span of it), so peak RSS ≈ inflated size +
//! events. This module parses large files in BLOCK-aligned windows instead:
//!
//! 1. `.gz` input is stream-decompressed into a temp file (decode and disk
//!    write overlap on two threads through a bounded channel); plain input
//!    is read in place. No ISIZE-sized output buffer is ever reserved.
//! 2. A parallel block-delta prescan (64KB blocks, `pread` slices) builds the
//!    exact global JSON structure states (`Blocks`), plus the `traceEvents`
//!    array bounds and the `metadata` spans — the same layout `scan_layout`
//!    computes, without mapping the whole file.
//! 3. Windows (4MB, BLOCK-aligned) are parsed in parallel with the same
//!    block-parallel walk/tokenizer as the whole-file path
//!    ([`chunk_work_stream_into`]): every window sees exact global block
//!    states, owns the elements starting inside its range, and completes a
//!    straddling tail element via lookahead re-reads (extended until it
//!    closes, so arbitrarily large events still parse exactly once).
//!    Per-window event counts from the prescan open table size disjoint
//!    slices of one events vec parsed in place, preserving file order with
//!    no merge pile.
//! 4. Retained `args` bytes are copied into per-worker [`ArgsArena`]s during
//!    a copy pass over each window; the window buffer is dropped right after.
//!    Peak RSS is events + arenas + a few inflight windows, never the file.
//!
//! Small files (< 16MB) stay on the existing whole-buffer path (`parse.rs`):
//! zero behavior change where memory was never a problem.

use super::event::ArgsArena;
use super::parse::{
    ArgsSpan, BLOCK, Blocks, ParseStats, chunk_work_stream_into, count_unescaped_quotes,
    parse_thread_count, skip_value, skip_ws, walk_range,
};
use super::{TraceEvent, TraceMetadata};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Files below this decoded size stay on the whole-buffer path (same
/// threshold that routes to the block-parallel parse in `parse.rs`).
pub(crate) const STREAM_THRESHOLD: usize = 16 * 1024 * 1024;

/// Window size for the streaming parse (a multiple of `BLOCK` so
/// boundaries after the first stay BLOCK-aligned with exact global states).
pub(crate) const WINDOW: usize = 4 * 1024 * 1024;

/// Head bytes probed for the `traceEvents` opener. Real headers are <1KB;
/// anything larger falls back to the whole-buffer path.
const HEAD_CAP: usize = 1024 * 1024;

/// Tail bytes (`arr_close..EOF`) retained for the `metadata` parse. Trailing
/// garbage past a small tail is ignored exactly like the whole-file path;
/// an absurdly large tail gives up on metadata (which the whole-file probe
/// also fails to close then).
const TAIL_CAP: usize = 16 * 1024 * 1024;

/// Random-access byte source: the input file itself (plain JSON), or a
/// stream-decompressed temp file (`.gz`) deleted on drop.
pub(crate) struct ByteSource {
    file: File,
    len: u64,
    /// Temp path to delete on drop (`None` for plain input).
    temp: Option<PathBuf>,
}

impl Drop for ByteSource {
    fn drop(&mut self) {
        if let Some(p) = self.temp.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl ByteSource {
    pub(crate) fn plain(path: &Path, file: File, len: u64) -> Self {
        let _ = path;
        Self {
            file,
            len,
            temp: None,
        }
    }

    /// Read exactly `buf.len()` bytes at `offset` (errors on short reads,
    /// like the whole-file exact reads).
    pub(crate) fn read_at_exact(&self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    /// Decoded length in bytes (temp file length for `.gz`, input length
    /// for plain JSON).
    pub(crate) fn decoded_len(&self) -> u64 {
        self.len
    }
}

/// Error context helper: parse/IO errors carry "path: message", matching the
/// whole-file `format!("{}: {}", path.display(), e)` mapping.
fn ctx(path: &Path, e: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    Box::<dyn std::error::Error>::from(format!("{}: {}", path.display(), e))
}

/// How the windowed parse gave up before reading events: exotic layouts
/// fall back to the whole-buffer path (same outcome as today), anything else
/// is a hard error with the whole-file message.
pub(crate) enum StreamFail {
    /// No array opener in the head / layout unrecognized: run the
    /// whole-buffer path instead (correct, memory-heavy, rare).
    Fallback,
    /// Hard error (message already carries `path: ` context).
    Fatal(Box<dyn std::error::Error>),
}

impl From<Box<dyn std::error::Error>> for StreamFail {
    fn from(e: Box<dyn std::error::Error>) -> Self {
        StreamFail::Fatal(e)
    }
}

/// Stream-decompress `gz_file` (`.gz` input) into a temp file, overlapping
/// decode and disk write on two threads through a bounded channel. Returns
/// the temp-backed [`ByteSource`]. Flate2 errors surface with the same
/// `"{path}: {message}"` shape as the whole-file gz path.
fn gunzip_to_temp(path: &Path, gz_file: File) -> Result<ByteSource, Box<dyn std::error::Error>> {
    use std::sync::mpsc::sync_channel;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "chperf-dec-{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let (tx, rx) = sync_channel::<Vec<u8>>(4);
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let t0 = std::time::Instant::now();
    let tmp_w = tmp.clone();
    std::thread::scope(|s| {
        let dec = s.spawn(move || -> Result<u64, String> {
            let mut dec = flate2::read::GzDecoder::new(gz_file);
            let mut total = 0u64;
            loop {
                let mut chunk = vec![0u8; 1024 * 1024];
                let mut filled = 0usize;
                while filled < chunk.len() {
                    match dec.read(&mut chunk[filled..]) {
                        Ok(0) => break,
                        Ok(n) => {
                            filled += n;
                            total += n as u64;
                        }
                        Err(e) => return Err(e.to_string()),
                    }
                }
                if filled == 0 {
                    break;
                }
                chunk.truncate(filled);
                if tx.send(chunk).is_err() {
                    break;
                }
            }
            Ok(total)
        });
        let out = s.spawn(move || -> Result<u64, String> {
            let f = File::create(&tmp_w).map_err(|e| e.to_string())?;
            let mut w = std::io::BufWriter::with_capacity(1024 * 1024, f);
            let mut total = 0u64;
            while let Ok(chunk) = rx.recv() {
                w.write_all(&chunk).map_err(|e| e.to_string())?;
                total += chunk.len() as u64;
            }
            w.flush().map_err(|e| e.to_string())?;
            Ok(total)
        });
        let dn: Result<u64, String> = dec
            .join()
            .unwrap_or_else(|_| Err("decode thread panicked".to_string()));
        let wn: Result<u64, String> = out
            .join()
            .unwrap_or_else(|_| Err("write thread panicked".to_string()));
        match (dn, wn) {
            (Ok(_), Ok(len)) => {
                if debug {
                    eprintln!(
                        "  [stream] gunzip {:.1}ms, {}MB",
                        t0.elapsed().as_secs_f64() * 1000.0,
                        len / 1_000_000
                    );
                }
                let file = File::open(&tmp).map_err(|e| {
                    Box::<dyn std::error::Error>::from(format!("{}: {}", path.display(), e))
                })?;
                Ok(ByteSource {
                    file,
                    len,
                    temp: Some(tmp),
                })
            }
            (Err(e), _) => {
                let _ = std::fs::remove_file(&tmp);
                Err(Box::<dyn std::error::Error>::from(format!(
                    "{}: {}",
                    path.display(),
                    e
                )))
            }
            (Ok(_), Err(e)) => {
                let _ = std::fs::remove_file(&tmp);
                Err(Box::<dyn std::error::Error>::from(format!(
                    "{}: {}",
                    path.display(),
                    e
                )))
            }
        }
    })
}

/// Parallel block prescan over `[0, len)`: per-64KB-block quote parity and
/// string-aware depth deltas, chained into exact global states — the same
/// `Blocks` `build_blocks` computes, without holding the bytes. Each worker
/// reads its blocks with `pread` into a reused buffer; only the tiny state
/// vecs (~10B/block) are retained.
fn scan_blocks_source(
    src: &ByteSource,
    len: usize,
    threads: usize,
) -> (Blocks, Vec<Vec<(i64, u32)>>) {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let t0 = std::time::Instant::now();
    let nb = len.div_ceil(BLOCK);
    let workers = threads.max(1).min(nb.max(1));
    // Phase 1: per-block quote parity + trailing backslash runs.
    let mut quote_parity = vec![false; nb];
    let mut trails = vec![0usize; nb];
    std::thread::scope(|s| {
        let ptr_q: usize = quote_parity.as_mut_ptr() as usize;
        let ptr_t: usize = trails.as_mut_ptr() as usize;
        for w in 0..workers {
            let (lo, hi) = (nb * w / workers, nb * (w + 1) / workers);
            let file = &src.file;
            s.spawn(move || {
                let ptr_q = ptr_q as *mut bool;
                let ptr_t = ptr_t as *mut usize;
                let mut buf = vec![0u8; BLOCK];
                // Seed: backslash run ending right before this worker's
                // range (a `\"` split across a worker boundary must count
                // exactly like the whole-file scan).
                let mut prev_trail = 0usize;
                if lo > 0 {
                    let back = (lo as u64 * BLOCK as u64).saturating_sub(4096);
                    let n = (lo as u64 * BLOCK as u64 - back) as usize;
                    let mut probe = vec![0u8; n];
                    if file.read_exact_at(&mut probe, back).is_ok() {
                        let mut k = probe.len();
                        while k > 0 && probe[k - 1] == b'\\' {
                            prev_trail += 1;
                            k -= 1;
                        }
                    }
                }
                for k in lo..hi {
                    let off = k as u64 * BLOCK as u64;
                    let n = ((len as u64 - off).min(BLOCK as u64)) as usize;
                    if file.read_exact_at(&mut buf[..n], off).is_err() {
                        break;
                    }
                    let (nq, trail) = count_unescaped_quotes(&buf[..n], prev_trail);
                    unsafe {
                        *ptr_q.add(k) = nq % 2 == 1;
                    }
                    unsafe {
                        *ptr_t.add(k) = trail;
                    }
                    prev_trail = trail;
                }
            });
        }
    });
    let mut in_string_at = Vec::with_capacity(nb + 1);
    let mut st = false;
    in_string_at.push(false);
    for par in &quote_parity {
        st ^= *par;
        in_string_at.push(st);
    }
    let mut esc_at = vec![false; nb];
    for k in 1..nb {
        esc_at[k] = in_string_at[k] && trails[k - 1] % 2 == 1;
    }
    // Phase 2: exact per-block depth deltas (string-aware walks) plus the
    // element-open table per block: `{` counts keyed by walk-relative depth
    // (absolute base unknown until the prefix sum below). Window event
    // counts attribute these at absolute depth 2 post-prefix (see
    // `window_counts`) — zero extra reads.
    let mut deltas = vec![0i64; nb];
    let mut opens: Vec<Vec<(i64, u32)>> = (0..nb).map(|_| Vec::new()).collect();
    std::thread::scope(|s| {
        let ptr_d: usize = deltas.as_mut_ptr() as usize;
        let ptr_o: usize = opens.as_mut_ptr() as usize;
        for w in 0..workers {
            let (lo, hi) = (nb * w / workers, nb * (w + 1) / workers);
            let file = &src.file;
            let states = in_string_at[lo..hi].to_vec();
            let escs = esc_at[lo..hi].to_vec();
            s.spawn(move || {
                let ptr_d = ptr_d as *mut i64;
                let ptr_o = ptr_o as *mut Vec<(i64, u32)>;
                let mut buf = vec![0u8; BLOCK];
                for (j, k) in (lo..hi).enumerate() {
                    let off = k as u64 * BLOCK as u64;
                    let n = ((len as u64 - off).min(BLOCK as u64)) as usize;
                    if file.read_exact_at(&mut buf[..n], off).is_err() {
                        break;
                    }
                    let mut dd: i64 = 0;
                    let mut sstate = states[j];
                    let mut esc = escs[j];
                    let no = walk_block_opens(&buf[..n], &mut dd, &mut sstate, &mut esc);
                    unsafe {
                        *ptr_d.add(k) = dd;
                    }
                    unsafe {
                        *ptr_o.add(k) = no;
                    }
                }
            });
        }
    });
    let mut depth_at = Vec::with_capacity(nb + 1);
    let mut d = 0i64;
    depth_at.push(0);
    for dd in &deltas {
        d += *dd;
        depth_at.push(d);
    }
    if debug {
        eprintln!(
            "  [stream] prescan {:.1}ms, {} blocks",
            t0.elapsed().as_secs_f64() * 1000.0,
            nb
        );
    }
    (
        Blocks {
            depth_at,
            in_string_at,
            esc_at,
        },
        opens,
    )
}

/// String-aware walk over one block: the same depth/state transitions as
/// [`walk_range`] (shared hot code stays untouched), additionally recording
/// `{` opens keyed by walk-relative depth (relative 0 = block-start depth;
/// the absolute base is only known after the depth prefix sum, so window
/// attribution maps these post-prefix — see `window_counts`). Entries are
/// (relative level, count) pairs, ascending, one per level that opened.
fn walk_block_opens(r: &[u8], d: &mut i64, s: &mut bool, esc: &mut bool) -> Vec<(i64, u32)> {
    let mut out: Vec<(i64, u32)> = Vec::new();
    if r.is_empty() {
        return out;
    }
    // Relative depth from the block start (mirrors `*d`, which accumulates
    // the absolute delta in parallel).
    let mut rel: i64 = 0;
    let mut in_str = *s;
    let mut e = *esc;
    for &b in r {
        if in_str {
            if e {
                e = false;
            } else if b == b'\\' {
                e = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' | b'[' => {
                    if b == b'{' {
                        // Merge by level (linear find: levels present are a
                        // handful — the array level plus args nesting — so
                        // this stays far below the byte-walk cost).
                        match out.iter_mut().find(|entry| entry.0 == rel) {
                            Some(entry) => entry.1 += 1,
                            None => out.push((rel, 1)),
                        }
                    }
                    rel += 1;
                    *d += 1;
                }
                b'}' | b']' => {
                    rel -= 1;
                    *d -= 1;
                }
                _ => {}
            }
        }
    }
    *s = in_str;
    *esc = e;
    out
}

/// Exact (string-aware) depth and in-string state at byte `pos`, walking at
/// most one block of `peek` (which holds file bytes `[peek_base, ...)`).
fn state_at_peek(blocks: &Blocks, pos: usize, peek: &[u8], peek_base: usize) -> (i64, bool) {
    let block = pos / BLOCK;
    let mut d = blocks.depth_at[block];
    let mut s = blocks.in_string_at[block];
    let mut esc = blocks.esc_at[block];
    let from = block * BLOCK;
    debug_assert!(from >= peek_base && pos <= peek_base + peek.len());
    walk_range(
        &peek[from - peek_base..pos - peek_base],
        &mut d,
        &mut s,
        &mut esc,
    );
    (d, s)
}

/// Locate the `traceEvents` array opener in the head bytes: the first
/// `"traceEvents"` member at depth 1 whose value opens with `[`. Structural
/// probes (member position + depth) match `scan_layout`'s contract; a
/// non-array value is genuinely non-trace input (the caller errors like the
/// whole-file path instead of mis-parsing).
fn head_array_open(
    head: &[u8],
    bom: usize,
    blocks: &Blocks,
) -> Result<Option<usize>, &'static str> {
    for m in memchr::memmem::find_iter(head, b"traceEvents") {
        if m == 0 || head[m - 1] != b'"' {
            continue;
        }
        let after = m + b"traceEvents".len();
        if after >= head.len() || head[after] != b'"' {
            continue;
        }
        let mut j = after + 1;
        while j < head.len() && head[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= head.len() || head[j] != b':' {
            continue;
        }
        j += 1;
        while j < head.len() && head[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= head.len() {
            continue;
        }
        // Byte before the key must be `{` or `,` (a member position, not
        // string content) — same probe as `find_trace_events_open`.
        let mut k = m - 1;
        while k > bom {
            k -= 1;
            if !head[k].is_ascii_whitespace() {
                break;
            }
        }
        if head[k] != b'{' && head[k] != b',' {
            continue;
        }
        let opener = head[j];
        if opener != b'[' && opener != b'{' {
            continue;
        }
        let (d, _) = state_at_peek(blocks, j, head, 0);
        if d != 1 {
            continue;
        }
        if opener != b'[' {
            return Err("non-array");
        }
        return Ok(Some(j));
    }
    Ok(None)
}

/// Array close `]`: the same backward walk `scan_layout` runs, over the last
/// ≤16 blocks (read once). `tail` holds file bytes `[tail_base, len)`;
/// `tail_base` is BLOCK-aligned so the walk's start states are exact.
fn tail_array_close(tail: &[u8], tail_base: usize, blocks: &Blocks, len: usize) -> Option<usize> {
    debug_assert!(tail_base % BLOCK == 0);
    let nb = blocks.depth_at.len() - 1;
    let first_block = tail_base / BLOCK;
    for k in (first_block..nb).rev() {
        let bs = k * BLOCK;
        let be = (bs + BLOCK).min(len);
        let mut d = blocks.depth_at[k];
        let mut s = blocks.in_string_at[k];
        let mut esc = blocks.esc_at[k];
        let mut i = bs;
        while i < be {
            let b = tail[i - tail_base];
            if s {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    s = false;
                }
            } else {
                match b {
                    b'"' => s = true,
                    b'{' | b'[' => d += 1,
                    b'}' | b']' => {
                        if b == b']' && d == 2 {
                            return Some(i);
                        }
                        d -= 1;
                    }
                    _ => {}
                }
            }
            i += 1;
        }
    }
    None
}

/// Metadata object `(open, end)` in the tail (global offsets): the same
/// probe + close walk as `scan_layout`, over retained tail bytes starting at
/// the BLOCK-aligned `tail_base`. `None` when absent, non-object, or
/// unclosed (truncated) — all matching the whole-file outcome.
fn tail_metadata_span(tail: &[u8], tail_base: usize, blocks: &Blocks) -> Option<(usize, usize)> {
    debug_assert!(tail_base % BLOCK == 0);
    for m in memchr::memmem::find_iter(tail, b"metadata") {
        if m == 0 || tail[m - 1] != b'"' {
            continue;
        }
        let after = m + b"metadata".len();
        if after >= tail.len() || tail[after] != b'"' {
            continue;
        }
        let mut j = after + 1;
        while j < tail.len() && tail[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= tail.len() || tail[j] != b':' {
            continue;
        }
        j += 1;
        while j < tail.len() && tail[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= tail.len() {
            continue;
        }
        let opener = tail[j];
        if opener != b'[' && opener != b'{' {
            continue;
        }
        let pos = tail_base + j;
        // Depth at the value opener, walked continuously from the tail start
        // (whose block state is exact — `tail_base` is BLOCK-aligned).
        let tblock = tail_base / BLOCK;
        let mut d = blocks.depth_at[tblock];
        let mut s = blocks.in_string_at[tblock];
        let mut esc = blocks.esc_at[tblock];
        walk_range(&tail[..j], &mut d, &mut s, &mut esc);
        if d != 1 {
            continue;
        }
        if opener != b'{' {
            return None;
        }
        // Continue the same walk past the `{` to its close at depth 2.
        d += 1;
        for (n, &b) in tail[j + 1..].iter().enumerate() {
            if s {
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    s = false;
                }
                continue;
            }
            match b {
                b'"' => s = true,
                b'{' | b'[' => d += 1,
                b'}' | b']' => {
                    if b == b'}' && d == 2 {
                        return Some((pos, pos + n + 2));
                    }
                    d -= 1;
                }
                _ => {}
            }
        }
        // Unclosed (truncated tail): no metadata, like the whole-file walk
        // running past EOF.
        return None;
    }
    None
}

/// Parse the top-level header `[bom, arr_open)` for a `metadata` object value:
/// small, serial, same tolerant outcome as the whole-file fast path (which
/// `serde`-parses the span and ignores failures).
fn head_metadata_span(head: &[u8], bom: usize, arr_open: usize) -> Option<(usize, usize)> {
    let mut p = skip_ws(head, bom);
    if p < head.len() && head[p] == b'{' {
        p += 1;
    }
    loop {
        p = skip_ws(head, p);
        if p >= arr_open || p >= head.len() {
            return None;
        }
        match head[p] {
            b'}' | b']' | b',' => {
                p += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        // Key bytes (unescaped; an escaped metadata key falls back the same
        // way the fast path bails — metadata is then simply absent here).
        let mut q = p + 1;
        let mut ok = true;
        while q < head.len() && head[q] != b'"' {
            if head[q] == b'\\' {
                ok = false;
                break;
            }
            q += 1;
        }
        if !ok || q >= head.len() {
            return None;
        }
        let key = &head[p + 1..q];
        p = skip_ws(head, q + 1);
        if p >= head.len() || head[p] != b':' {
            return None;
        }
        p = skip_ws(head, p + 1);
        if p >= head.len() {
            return None;
        }
        if key == b"metadata" {
            if head[p] != b'{' {
                return None;
            }
            return skip_value(head, p).ok().map(|e| (p, e.min(arr_open)));
        }
        // Skip any other pre-array value (same as the fast path).
        match skip_value(head, p) {
            Ok(n) => p = n,
            Err(_) => return None,
        }
    }
}

/// Window boundaries over `[arr_open, arr_end)`: window 0 starts at the
/// array `[` (its header is skipped by ownership); every later boundary is
/// BLOCK-aligned, so at most the first block (pre-array header braces) and
/// the last block (post-close tail braces) over-count — a handful of events
/// that compact away. Returns `n_windows + 1` offsets starting at
/// `arr_open` and ending at `arr_end`.
fn window_bounds(arr_open: usize, arr_end: usize) -> Vec<usize> {
    let mut bounds = vec![arr_open];
    while *bounds.last().unwrap() < arr_end {
        let last = *bounds.last().unwrap();
        // Nominal end, pulled back to a block boundary (except the array end
        // itself, which may sit mid-block in the tail). `WINDOW` is a
        // multiple of `BLOCK`, so aligned windows stay full-sized.
        let next = ((last + WINDOW) / BLOCK * BLOCK).min(arr_end).max(last + 1);
        bounds.push(next);
    }
    bounds
}

/// Count `{` opens at walk-relative level `want` in `r` (same string-aware
/// transitions as [`walk_range`): corrects window 0's first (partial) block,
/// whose pre-array header bytes (e.g. nested `metadata` objects) the
/// block table counts but no window owns.
fn count_opens_range(r: &[u8], s0: bool, esc0: bool, want: i64) -> usize {
    let mut in_str = s0;
    let mut e = esc0;
    let mut rel: i64 = 0;
    let mut c = 0usize;
    for &b in r {
        if in_str {
            if e {
                e = false;
            } else if b == b'\\' {
                e = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' | b'[' => {
                    if b == b'{' && rel == want {
                        c += 1;
                    }
                    rel += 1;
                }
                b'}' | b']' => rel -= 1,
                _ => {}
            }
        }
    }
    c
}
/// Per-window event counts from the prescan open table: window `k` over
/// `[bounds[k], bounds[k + 1])` owns the elements opening in its block
/// range. Attribution is post-prefix: block `b` starts at absolute depth
/// `depth_at[b]`, so its table entries at relative level `2 - depth_at[b]`
/// are array-item opens. The count never falls short of the walk's owned
/// completions (edge blocks only add foreign opens), so in-place slices
/// always suffice and any shortfall compacts away.
fn window_counts(opens: &[Vec<(i64, u32)>], depth_at: &[i64], bounds: &[usize]) -> Vec<usize> {
    let nb = opens.len();
    let mut out = Vec::with_capacity(bounds.len().saturating_sub(1));
    for k in 0..bounds.len().saturating_sub(1) {
        let (w0, w1) = (bounds[k], bounds[k + 1]);
        let b0 = (w0 / BLOCK).min(nb);
        let b1 = (w1.div_ceil(BLOCK)).min(nb).max(b0);
        let mut c = 0usize;
        for b in b0..b1 {
            let want = 2 - depth_at[b];
            for &(rel, n) in &opens[b] {
                if rel == want {
                    c += n as usize;
                }
            }
        }
        out.push(c);
    }
    out
}

/// Parse all windows in `[arr_open, arr_end)` in parallel (≤6 workers, like
/// the whole-file path) DIRECTLY into disjoint slices of the final events
/// vec — no per-window vecs, no merge pile. `counts` (see `window_counts`)
/// size each slice; the actual counts may fall short (tolerant skips,
/// truncated tails) and compact away afterwards.
///
/// Each worker reads its windows with tiered lookahead (64KB, then ×64 —
/// a straddling element almost always closes within the first tier),
/// tokenizes with [`chunk_work_stream_into`], and copies retained `args`
/// into its own [`ArgsArena`]. Returns per-window `(actual, stats)` in file
/// order.
#[allow(clippy::too_many_arguments)]
fn parse_windows_into(
    src: &ByteSource,
    blocks: &Blocks,
    arr_end: usize,
    threads: usize,
    bounds: &[usize],
    counts: &[usize],
    events: &mut [TraceEvent],
) -> Result<(Vec<usize>, Vec<ParseStats>), Box<dyn std::error::Error>> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    let t0 = std::time::Instant::now();
    let n_windows = counts.len();
    if n_windows == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let workers = threads.max(1).min(n_windows).min(6);
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cursor = AtomicUsize::new(0);
    // Disjoint slices, one per window, handed out under a mutex (taking is
    // O(1); 6 workers × ~20 takes each — no contention to speak of).
    let mut slices: Vec<Option<&mut [TraceEvent]>> = Vec::with_capacity(n_windows);
    {
        let mut rest: &mut [TraceEvent] = events;
        for k in 0..n_windows {
            let n = counts[k];
            let (head, tail) = rest.split_at_mut(n);
            rest = tail;
            slices.push(Some(head));
        }
        debug_assert!(rest.is_empty());
    }
    let slices = std::sync::Mutex::new(slices);
    let actuals: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(vec![0usize; n_windows]);
    let stats: std::sync::Mutex<Vec<ParseStats>> =
        std::sync::Mutex::new((0..n_windows).map(|_| ParseStats::default()).collect());
    let arena_bytes = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let mut arena = ArgsArena::new();
                let mut spans: Vec<ArgsSpan> = Vec::new();
                // Reused scratch: window reads, span tables and the pending
                // copy list keep their allocations across windows instead of
                // churning multi-MB buffers hundreds of times.
                let mut buf: Vec<u8> = Vec::new();
                let mut pending: Vec<(u32, u32, u32, u32)> = Vec::new();
                loop {
                    let idx = cursor.fetch_add(1, Ordering::Relaxed);
                    if idx >= n_windows {
                        break;
                    }
                    let (w0, w1) = (bounds[idx], bounds[idx + 1]);
                    let mut slice = slices.lock().unwrap()[idx]
                        .take()
                        .expect("slice taken once");
                    match parse_one_window_into(
                        src,
                        blocks,
                        w0,
                        w1,
                        arr_end,
                        &mut arena,
                        &mut spans,
                        &mut buf,
                        &mut pending,
                        &mut slice,
                    ) {
                        Ok((actual, st)) => {
                            actuals.lock().unwrap()[idx] = actual;
                            stats.lock().unwrap()[idx] = st;
                        }
                        Err(e) => {
                            // A window never fails the file on its own: an
                            // unreadable window degrades to zero events (the
                            // empty-trace error fires downstream when nothing
                            // parsed at all). Record the debug note only.
                            if debug {
                                eprintln!("  [stream] window {idx} failed: {e}");
                            }
                        }
                    }
                }
                arena_bytes.fetch_add(arena.retained_bytes(), Ordering::Relaxed);
            });
        }
    });
    if debug {
        let total: usize = actuals.lock().unwrap().iter().sum();
        eprintln!(
            "  [stream] windows {:.1}ms: {} events in {} windows; arenas={}MB",
            t0.elapsed().as_secs_f64() * 1000.0,
            total,
            n_windows,
            arena_bytes.load(Ordering::Relaxed) / 1_000_000
        );
    }
    Ok((actuals.into_inner().unwrap(), stats.into_inner().unwrap()))
}

/// Parse one window `[w0, w1)` (global offsets, `w1` already clamped to the
/// array end) into `out`. The buffer starts at the block containing `w0` so
/// the walk's start state is exact; ownership starts at `w0` (window 0's
/// `w0` is the array `[`, so its header is skipped by ownership). Tiered
/// lookahead re-reads (64KB, then ×64) extend until the tail element closes.
#[allow(clippy::too_many_arguments)]
fn parse_one_window_into(
    src: &ByteSource,
    blocks: &Blocks,
    w0: usize,
    w1: usize,
    arr_end: usize,
    arena: &mut ArgsArena,
    spans: &mut Vec<ArgsSpan>,
    buf: &mut Vec<u8>,
    pending: &mut Vec<(u32, u32, u32, u32)>,
    out: &mut [TraceEvent],
) -> Result<(usize, ParseStats), String> {
    let from_g = w0;
    let to_g = w1.min(arr_end);
    // Buffer starts at the block containing `from` (window 0's `from` is the
    // array `[`, which may sit mid-block after the header).
    let buf_base = from_g / BLOCK * BLOCK;
    let mut look = 64 * 1024usize;
    loop {
        let read_end = (w1 + look).min(arr_end).max(to_g);
        let n = read_end - buf_base;
        buf.resize(n, 0);
        src.file
            .read_exact_at(buf, buf_base as u64)
            .map_err(|e| format!("read window {buf_base}..{read_end}: {e}"))?;
        spans.clear();
        let (actual, stats, consumed, tail_open) =
            chunk_work_stream_into(buf, buf_base, blocks, from_g, to_g, spans, out)?;
        // Straddler needs more bytes: extend and retry (always terminates:
        // `read_end` grows to `arr_end`, where the walk must settle).
        if tail_open && consumed < to_g && read_end < arr_end {
            look = look.saturating_mul(64).max(1);
            continue;
        }
        let _ = consumed;
        return resolve_window_into(actual, spans, buf, arena, pending, out).map(|a| (a, stats));
    }
}

/// Copy pass: append every recorded `args` span to the worker arena and set
/// the event's `args` in the window slice. `actual` slots were written;
/// returns it unchanged (tolerant-path events already own their `args` —
/// small dedicated buffers, cold path — and never appear in `spans`).
fn resolve_window_into(
    actual: usize,
    spans: &[ArgsSpan],
    buf: &[u8],
    arena: &mut ArgsArena,
    pending: &mut Vec<(u32, u32, u32, u32)>,
    out: &mut [TraceEvent],
) -> Result<usize, String> {
    // Appends first (the current chunk is still growing and unsealed), then
    // seal once and resolve every recorded triple against sealed chunks.
    pending.clear();
    pending.reserve(spans.len());
    for &(idx, s, e) in spans {
        let raw = std::str::from_utf8(&buf[s as usize..e as usize])
            .map_err(|_| "args span not UTF-8".to_string())?;
        let (seq, astart, alen) = arena.append(raw.as_bytes());
        pending.push((idx, seq, astart, alen));
    }
    arena.seal();
    for &(idx, seq, astart, alen) in pending.iter() {
        out[idx as usize].args = Some(arena.resolve(seq, astart, alen));
    }
    Ok(actual)
}

/// Streaming parse of a large file behind `src` (decoded length `len`).
/// Returns the events (file order), top-level metadata, and parse counters —
/// the same triple `parse_parallel` yields, with none of the file retained.
pub(crate) fn parse_stream(
    src: &ByteSource,
    path: &Path,
    len: usize,
) -> Result<(Vec<TraceEvent>, Option<TraceMetadata>, ParseStats), StreamFail> {
    let debug = std::env::var("CHPERF_DEBUG").is_ok();
    // Head: BOM + `traceEvents` opener probe.
    let head_n = len.min(HEAD_CAP);
    let mut head = vec![0u8; head_n];
    src.read_at_exact(&mut head, 0).map_err(|e| ctx(path, e))?;
    let bom = if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    // Prescan first (block states back the opener depth probe; the
    // per-block open counts size the in-place event slices).
    let threads = parse_thread_count(&len);
    let (blocks, opens) = scan_blocks_source(src, len, threads);
    let arr_open = match head_array_open(&head, bom, &blocks) {
        Ok(Some(o)) => o,
        // No top-level array in the head: exotic layout — the caller falls
        // back to the whole-buffer path (same outcome as today).
        Ok(None) => return Err(StreamFail::Fallback),
        Err(_) => {
            // Non-array `traceEvents` value: genuinely non-trace input.
            return Err(StreamFail::Fatal(Box::<dyn std::error::Error>::from(
                format!(
                    "{}: not a Chrome trace (missing top-level \"traceEvents\" array)",
                    path.display()
                ),
            )));
        }
    };
    if arr_open + 1 >= len {
        return Err(StreamFail::Fallback);
    }
    if debug {
        eprintln!("  [stream] arr_open at {arr_open}");
    }
    // Array close: backward walk over the last ≤16 blocks (block-aligned
    // base, so the walk's start states are exact).
    let probe_n = len.min(16 * BLOCK);
    let probe_base = (len - probe_n) / BLOCK * BLOCK;
    let mut tail_probe = vec![0u8; len - probe_base];
    src.read_at_exact(&mut tail_probe, probe_base as u64)
        .map_err(|e| ctx(path, e))?;
    let arr_close = tail_array_close(&tail_probe, probe_base, &blocks, len);
    // EOF-bounded array end: the close, or EOF when truncated (salvage the
    // complete-event prefix exactly like the tolerant path).
    let arr_end = arr_close.unwrap_or(len);
    if arr_end <= arr_open {
        return Err(StreamFail::Fallback);
    }
    // Tail bytes for the metadata parse (BLOCK-aligned base, capped; beyond
    // the cap metadata is None — the whole-file probe also fails to close
    // it then).
    let tail_keep_from = (arr_end.min(len) / BLOCK * BLOCK).min(len);
    let tail_keep_n = (len - tail_keep_from).min(TAIL_CAP + BLOCK);
    let mut tail = vec![0u8; tail_keep_n];
    if tail_keep_n > 0 {
        src.read_at_exact(&mut tail, tail_keep_from as u64)
            .map_err(|e| ctx(path, e))?;
    }
    let meta_tail = tail_metadata_span(&tail, tail_keep_from, &blocks);
    let meta_head = if head.len() > arr_open {
        head_metadata_span(&head, bom, arr_open)
    } else {
        None
    };
    let metadata: Option<TraceMetadata> = meta_head
        .and_then(|(s, e)| serde_json::from_slice::<TraceMetadata>(&head[s..e]).ok())
        .or_else(|| {
            meta_tail.and_then(|(s, e)| {
                serde_json::from_slice::<TraceMetadata>(
                    &tail[s - tail_keep_from..e - tail_keep_from],
                )
                .ok()
            })
        });
    if debug {
        eprintln!(
            "  [stream] arr_end at {arr_end}, metadata={}",
            metadata.is_some()
        );
    }
    // Layout inputs (head, tail probe, tail) are fully consumed: drop them
    // before the window parse so only events + arenas + windows peak.
    drop(head);
    drop(tail_probe);
    drop(tail);
    // Windows over the array content (window 0 starts at the array `[` so
    // the header is skipped by ownership), parsed in place: counts from the
    // prescan open table size disjoint slices of one events vec, workers
    // tokenize into their slices, and any shortfall (tolerant skips,
    // truncated tails) compacts away. No per-window vecs, no merge pile.
    let content_start = arr_open;
    let bounds = window_bounds(content_start, arr_end);
    let n_windows = bounds.len() - 1;
    let mut counts = window_counts(&opens, &blocks.depth_at, &bounds);
    // Window 0's first block is partial (it starts before the array `[`):
    // subtract the pre-array opens it counted (nested `metadata` objects —
    // real traces carry e.g. a `modifications` object at depth 2). Every
    // open before `arr_open` is foreign to all windows, so this is exact.
    {
        let b0 = content_start / BLOCK * BLOCK;
        if b0 < content_start && !counts.is_empty() {
            let n = content_start - b0;
            let mut pre = vec![0u8; n];
            src.read_at_exact(&mut pre, b0 as u64)
                .map_err(|e| ctx(path, e))?;
            let bi = b0 / BLOCK;
            let foreign = count_opens_range(
                &pre,
                blocks.in_string_at[bi],
                blocks.esc_at[bi],
                2 - blocks.depth_at[bi],
            );
            counts[0] = counts[0].saturating_sub(foreign);
        }
    }
    // The open table served its purpose: drop it before the window parse.
    drop(opens);
    let total: usize = counts.iter().sum();
    if debug {
        eprintln!("  [stream] reserved slots: {total}");
    }
    let t_init = std::time::Instant::now();
    let mut events: Vec<TraceEvent> = vec![TraceEvent::default(); total];
    if debug {
        eprintln!(
            "  [stream] init {:.1}ms",
            t_init.elapsed().as_secs_f64() * 1000.0
        );
    }
    let (actuals, win_stats) = parse_windows_into(
        src,
        &blocks,
        arr_end,
        threads,
        &bounds,
        &counts,
        &mut events,
    )?;
    // Compact shortfalls (actual < reserved): second prefix over actuals,
    // forward memmove per window (new offsets never exceed old ones), then
    // truncate. Clean traces never shortfall — this is free then.
    let actual_total: usize = actuals.iter().sum();
    let mut stats = ParseStats::default();
    for st in &win_stats {
        stats.parsed += st.parsed;
        stats.skipped += st.skipped;
        stats.malformed += st.malformed;
    }
    if actual_total != total {
        // Remove each window's hole (reserved tail the window didn't fill):
        // `drain` drops the dummy slots and shifts the tail left with
        // Drop-correct std internals. Only exotic traces shortfall — clean
        // traces skip this entirely.
        if debug {
            for k in 0..n_windows {
                if counts[k] != actuals[k] {
                    eprintln!(
                        "  [stream] hole: window {k} reserved={} actual={}",
                        counts[k], actuals[k]
                    );
                }
            }
        }
        let mut offsets = Vec::with_capacity(n_windows + 1);
        offsets.push(0usize);
        for c in &counts {
            offsets.push(offsets.last().copied().unwrap_or(0) + *c);
        }
        let mut shift = 0usize;
        for k in 0..n_windows {
            // Overruns are structurally impossible (both tokenizers bound
            // writes by the slice via `get_mut`), so holes only shrink.
            debug_assert!(actuals[k] <= counts[k]);
            let hole = counts[k] - actuals[k];
            if hole > 0 {
                let at = offsets[k] + actuals[k] - shift;
                events.drain(at..at + hole);
                shift += hole;
            }
        }
        debug_assert_eq!(events.len(), actual_total);
    }
    if debug {
        eprintln!(
            "  [stream] in-place: reserved={} actual={}",
            total, actual_total
        );
    }
    Ok((events, metadata, stats))
}

/// Read up to `cap` bytes from `r`; returns the bytes plus whether EOF was
/// reached (i.e. the whole input fit).
fn read_capped<R: Read>(mut r: R, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut out = Vec::with_capacity(cap.min(1024 * 1024));
    let mut buf = [0u8; 64 * 1024];
    loop {
        if out.len() >= cap {
            return Ok((out, false));
        }
        let want = (cap - out.len()).min(buf.len());
        match r.read(&mut buf[..want]) {
            Ok(0) => return Ok((out, true)),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e),
        }
    }
}

/// Dispatch helper for `parse_trace`: stream-decodes a `.gz` file, returning
/// either the full small bytes or a temp-backed source for large input.
pub(crate) fn open_source(
    path: &Path,
    is_gz: bool,
) -> Result<SourceKind, Box<dyn std::error::Error>> {
    if !is_gz {
        let file = File::open(path).map_err(|e| ctx(path, e))?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < STREAM_THRESHOLD as u64 {
            return Ok(SourceKind::SmallPlain(file));
        }
        return Ok(SourceKind::Stream(ByteSource::plain(path, file, len)));
    }
    let file = File::open(path).map_err(|e| ctx(path, e))?;
    // Bounded sniff: small gz inputs stay fully in RAM (no temp file).
    let probe = File::open(path).map_err(|e| ctx(path, e))?;
    let (bytes, eof) = read_capped(flate2::read::GzDecoder::new(probe), STREAM_THRESHOLD + 1)
        .map_err(|e| ctx(path, e))?;
    // A BOM prefix shifts every downstream offset; strip it the same way
    // the whole-file path does (copy-once, cold path only).
    let bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes[3..].to_vec()
    } else {
        bytes
    };
    if eof {
        // `bytes` may hold exactly STREAM_THRESHOLD+1 only when the input
        // is that big — `eof` says it all fit, so this is small.
        return Ok(SourceKind::SmallGz(bytes));
    }
    drop(file);
    let file = File::open(path).map_err(|e| ctx(path, e))?;
    Ok(SourceKind::Stream(gunzip_to_temp(path, file)?))
}

/// What `open_source` found: small inputs decoded in RAM, large ones as a
/// random-access source for the windowed parse.
pub(crate) enum SourceKind {
    SmallPlain(File),
    SmallGz(Vec<u8>),
    Stream(ByteSource),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_big(name: &str, bytes: &[u8]) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("chperf-stream-{name}-{}.json", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn write_big_gz(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "chperf-stream-{name}-{}.json.gz",
            std::process::id()
        ));
        let f = File::create(&path).unwrap();
        let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
        enc.write_all(bytes).unwrap();
        enc.finish().unwrap();
        path
    }

    /// Deterministic pseudo-random bytes (LCG) exercising strings, escapes,
    /// braces and brackets at every alignment.
    fn prng_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        let mut out = Vec::with_capacity(len);
        const ALPHA: &[u8] = b"{}[]\"\\abc:, 0123456789 Trufal";
        for _ in 0..len {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push(ALPHA[(x >> 33) as usize % ALPHA.len()]);
        }
        out
    }

    /// `walk_block_opens` deltas must agree with the shared `walk_range`
    /// exactly (same transitions), on hostile bytes at every block split.
    #[test]
    fn block_opens_agree_with_walk_range() {
        for seed in [1u64, 7, 12345] {
            let bytes = prng_bytes(3 * BLOCK + 123, seed);
            for split in [0, 1, BLOCK - 1, BLOCK, BLOCK + 1, 2 * BLOCK + 17] {
                let (head, tail) = bytes.split_at(split);
                // Reference: whole-buffer walk.
                let mut d_ref = 0i64;
                let mut s_ref = false;
                let mut e_ref = false;
                walk_range(head, &mut d_ref, &mut s_ref, &mut e_ref);
                let mut d_tail = d_ref;
                let mut s_tail = s_ref;
                let mut e_tail = e_ref;
                walk_range(tail, &mut d_tail, &mut s_tail, &mut e_tail);
                // Split walk with the counting variant on the tail.
                let mut d2 = d_ref;
                let mut s2 = s_ref;
                let mut e2 = e_ref;
                let opens = walk_block_opens(tail, &mut d2, &mut s2, &mut e2);
                assert_eq!(
                    (d2, s2, e2),
                    (d_tail, s_tail, e_tail),
                    "seed={seed} split={split}"
                );
                // Opens sanity: total `{` at rel>=0 equals the block's `{`
                // count outside strings only when depth never goes negative
                // — just check the table is internally consistent.
                let sum: usize = opens.iter().map(|(_, n)| *n as usize).sum();
                assert!(sum <= tail.iter().filter(|b| **b == b'{').count());
            }
        }
    }

    /// Window bounds always progress and tile `[arr_open, arr_end)` exactly.
    #[test]
    fn window_bounds_tile() {
        for (open, end) in [(595usize, 959070360usize), (0, 100), (17, 17 + BLOCK)] {
            let b = window_bounds(open, end);
            assert_eq!(*b.first().unwrap(), open);
            assert_eq!(*b.last().unwrap(), end);
            for w in b.windows(2) {
                assert!(w[0] < w[1], "no progress at {:?}", w);
            }
            // All interior boundaries BLOCK-aligned.
            for m in &b[1..b.len() - 1] {
                assert_eq!(m % BLOCK, 0, "unaligned interior {m}");
            }
        }
    }

    /// Build a big trace exercising: escapes, a >1MB giant event straddling
    /// windows, async ids, B/E pairs, metadata before AND after the array,
    /// thread names, and TracingStartedInBrowser frames.
    fn big_doc(n: usize) -> Vec<u8> {
        let mut body = String::from("{\"traceEvents\":[");
        for i in 0..n {
            if i > 0 {
                body.push(',');
            }
            match i % 13 {
                0 => body.push_str(&format!(
                    "{{\"name\":\"Ev \\\"q{i}\\\"\",\"ph\":\"X\",\"ts\":{i},\"pid\":1,\"tid\":1,\"dur\":5}}"
                )),
                1 => body.push_str(&format!(
                    "{{\"name\":\"Big{i}\",\"ph\":\"X\",\"ts\":{i},\"pid\":1,\"tid\":1,\"args\":{{\"blob\":\"{}\"}}}}",
                    "B".repeat(1_300_000)
                )),
                2 => body.push_str(&format!(
                    "{{\"name\":\"A\",\"ph\":\"s\",\"ts\":{i},\"pid\":1,\"tid\":1,\"id2\":{{\"local\":\"0xABC\"}}}}"
                )),
                3 => body.push_str(&format!(
                    "{{\"name\":\"RunTask\",\"ph\":\"B\",\"ts\":{i},\"pid\":1,\"tid\":7}}"
                )),
                4 => body.push_str(&format!(
                    "{{\"name\":\"RunTask\",\"ph\":\"E\",\"ts\":{i},\"pid\":1,\"tid\":7}}"
                )),
                5 => body.push_str(&format!(
                    "{{\"name\":\"thread_name\",\"ph\":\"M\",\"ts\":0,\"pid\":1,\"tid\":1,\"args\":{{\"name\":\"CrRendererMain\"}}}}"
                )),
                6 => body.push_str(&format!(
                    "{{\"name\":\"TracingStartedInBrowser\",\"ph\":\"I\",\"ts\":{i},\"pid\":1,\"tid\":1,\"args\":{{\"data\":{{\"frames\":[{{\"frame\":\"F1\",\"url\":\"https://a.example/\",\"processId\":1,\"isInPrimaryMainFrame\":true}}]}}}}}}"
                )),
                _ => body.push_str(&format!(
                    "{{\"name\":\"Ev{}\",\"ph\":\"X\",\"ts\":{},\"pid\":1,\"tid\":1,\"dur\":1,\"cat\":\"c\",\"args\":{{\"v\":{}}}}}",
                    i % 50,
                    i,
                    i
                )),
            }
        }
        body.push_str("],\"metadata\":{\"cpuThrottling\":4}}");
        body.into_bytes()
    }

    /// Independent oracle: parse the same doc with serde's DOM and compare
    /// event counts + sampled fields (name/ts/args round-trip).
    fn dom_events(doc: &[u8]) -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_slice(doc).unwrap();
        v["traceEvents"].as_array().unwrap().clone()
    }

    /// The streaming path (in-place, windows, arenas) agrees with the DOM
    /// oracle on a >16MB trace, plain and gzipped.
    #[test]
    fn stream_matches_dom_oracle_plain_and_gz() {
        let doc = big_doc(9000);
        assert!(doc.len() > STREAM_THRESHOLD, "must take stream path");
        for (tag, path) in [
            ("plain", write_big("oracle", &doc)),
            ("gz", write_big_gz("oracle", &doc)),
        ] {
            let trace = crate::trace::parse_trace(&path).expect("stream test parse");
            let dom = dom_events(&doc);
            // B/E pairs synthesize 2 X per (B,E) RunTask pair... count raw:
            // every input event parses (none skipped), paired or not.
            assert_eq!(trace.stats.parsed, dom.len(), "{tag}: parsed");
            assert_eq!(trace.stats.skipped, 0, "{tag}: skipped");
            // Names/ts/order agree element-for-element over the raw prefix
            // (synthetics append after).
            for (i, d) in dom.iter().enumerate() {
                let e = &trace.trace_events[i];
                assert_eq!(
                    e.name.as_str(),
                    d["name"].as_str().unwrap(),
                    "{tag} ev {i} name"
                );
                assert_eq!(e.ts, d["ts"].as_f64().unwrap(), "{tag} ev {i} ts");
                match d.get("args") {
                    Some(a) if !a.is_null() => {
                        assert_eq!(e.args_value().as_ref(), Some(a), "{tag} ev {i} args");
                    }
                    _ => assert!(e.args_raw().is_none(), "{tag} ev {i} no args"),
                }
            }
            // Post-array metadata wins (cpuThrottling), frames extracted.
            let meta = trace.metadata.as_ref().expect("{tag}: metadata");
            assert_eq!(meta.cpu_throttling, Some(4.0));
            assert!(!meta.frames.is_empty(), "{tag}: frames");
            assert_eq!(meta.page_url.as_deref(), Some("https://a.example/"));
            let _ = std::fs::remove_file(&path);
        }
    }

    /// Truncated big trace salvages the complete-event prefix (late
    /// truncation) and errors clearly (early truncation).
    #[test]
    fn stream_truncated_big_salvages_prefix() {
        let doc = big_doc(9000);
        // Cut mid-event near the end: prefix salvages.
        let cut = doc.len() - 100;
        let path = write_big("trunc-late", &doc[..cut]);
        let trace = crate::trace::parse_trace(&path).expect("stream test parse");
        assert!(trace.stats.parsed > 8000, "got {}", trace.stats.parsed);
        let _ = std::fs::remove_file(&path);
        // Cut before any complete event: clear "no events" error.
        let open = super::super::parse::find_trace_events_open(&doc).unwrap();
        let path2 = write_big("trunc-early", &doc[..open + 10]);
        let err = crate::trace::parse_trace(&path2)
            .err()
            .expect("must fail")
            .to_string();
        assert!(err.contains("no events"), "unexpected: {err}");
        let _ = std::fs::remove_file(&path2);
    }

    /// `parse_windows_into` shortfall path: a window full of broken events
    /// compacts exactly (no holes, order kept, stats exact).
    #[test]
    fn stream_compact_skips_broken_keeps_order() {
        let mut parts: Vec<String> = Vec::with_capacity(3000);
        for i in 0..3000 {
            if i % 7 == 3 {
                parts.push(r#"{"name": }"#.to_string());
            } else {
                parts.push(format!(
                    "{{\"name\":\"E{i}\",\"ph\":\"X\",\"ts\":{i},\"pid\":1,\"tid\":1}}"
                ));
            }
        }
        // Pad past the stream threshold with more good events.
        let mut big = format!("{{\"traceEvents\":[{}", parts.join(","));
        let mut pads = 0usize;
        while big.len() < STREAM_THRESHOLD + 1024 {
            big.push_str(r#",{"name":"Pad","ph":"X","ts":1,"pid":1,"tid":1}"#);
            pads += 1;
        }
        big.push_str("]}");
        let path = write_big("compact", big.as_bytes());
        let trace = crate::trace::parse_trace(&path).expect("stream test parse");
        // Survivors keep file order (ts == position among E-events).
        let got: Vec<f64> = trace
            .trace_events
            .iter()
            .filter(|e| e.name.as_str() != "Pad")
            .map(|e| e.ts)
            .collect();
        let want: Vec<f64> = (0..3000).filter(|i| i % 7 != 3).map(|i| i as f64).collect();
        assert_eq!(got, want);
        // Exact accounting: every input element parsed or skipped exactly
        // once (429 broken of the first 3000).
        assert_eq!(trace.stats.skipped, 429);
        assert_eq!(trace.stats.parsed, 3000 - 429 + pads);
        assert_eq!(trace.stats.parsed + trace.stats.skipped, 3000 + pads);
    }

    /// ArgsText arena model: appended bytes resolve exactly, oversized
    /// values get dedicated chunks, seal-then-resolve is exact.
    #[test]
    fn arena_append_resolve_roundtrip() {
        use super::super::event::{ARENA_CHUNK, ArgsArena};
        let mut a = ArgsArena::new();
        let (s1, a1, l1) = a.append(br#"{"x":1}"#);
        let (s2, a2, l2) = a.append(br#"{"y":[1,2,3]}"#);
        let big = vec![b'z'; ARENA_CHUNK + 10];
        let (sb, ab, lb) = a.append(&big);
        a.seal();
        assert_eq!(a.resolve(s1, a1, l1).as_str(), Some(r#"{"x":1}"#));
        assert_eq!(a.resolve(s2, a2, l2).as_str(), Some(r#"{"y":[1,2,3]}"#));
        assert_eq!(
            a.resolve(sb, ab, lb).as_str().map(|s| s.len()),
            Some(big.len())
        );
        assert!(a.retained_bytes() >= 7 + 13 + big.len());
    }

    /// Metadata BEFORE the array (with nesting): the head parse keeps it on
    /// the stream path, and all events still parse.
    #[test]
    fn stream_pre_array_metadata_parsed() {
        let mut body = String::from(
            "{\"metadata\":{\"cpuThrottling\":2,\"nested\":{\"a\":[1]}},\"traceEvents\":[",
        );
        for i in 0..2000 {
            if i > 0 {
                body.push(',');
            }
            body.push_str(&format!(
                "{{\"name\":\"E{i}\",\"ph\":\"X\",\"ts\":{i},\"pid\":1,\"tid\":1}}"
            ));
        }
        while body.len() < STREAM_THRESHOLD + 1024 {
            body.push_str(r#",{"name":"Pad","ph":"X","ts":1,"pid":1,"tid":1}"#);
        }
        body.push_str("]}");
        let path = write_big("premeta", body.as_bytes());
        let trace = crate::trace::parse_trace(&path).expect("stream test parse");
        let _ = std::fs::remove_file(&path);
        assert!(trace.trace_events.len() > 2000);
        assert_eq!(
            trace.metadata.as_ref().and_then(|m| m.cpu_throttling),
            Some(2.0)
        );
    }
}
