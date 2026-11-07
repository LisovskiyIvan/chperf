//! Inline source maps from trace metadata: resolve minified CPU-profile
//! frames back to original `.ts` sources.
//!
//! Chrome DevTools "enhancedTrace" v1 traces carry
//! `metadata.sourceMaps: [{ url, sourceMap: { mappings, names, sources,
//! sourcesContent, … } }]`. `sourcesContent` is the bulk and is never
//! retained — [`crate::trace::TraceMetadata`] only keeps `mappings`,
//! `names` and `sources`.
//!
//! A [`SourceMapResolver`] is built over `&[SourceMapEntry]` (url → entry,
//! case-exact). Each map's `mappings` string is decoded lazily on first
//! query into per-generated-line sorted segments and cached (plain
//! `HashMap`, no LRU; a sane cap bounds worst-case memory). Malformed
//! base64/VLQ marks that map bad — resolution returns `None` forever and
//! never panics.

use super::meta::SourceMapEntry;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, RwLock};

/// Max decoded maps held in the cache. Entries are small (vite dev maps);
/// beyond the cap resolution just misses instead of growing memory.
const MAX_DECODED: usize = 512;

/// One decoded segment on a generated line.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seg {
    /// Generated column (sorted ascending within the line).
    gen_col: u32,
    /// `None` for unmapped (1-field) segments — the nearest segment at or
    /// before the query column, but with no source to resolve to.
    src: Option<SegSrc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SegSrc {
    src_idx: u32,
    src_line: u32,
    name_idx: Option<u32>,
}

/// Decoded map: per-generated-line segments plus the `sources`/`names`
/// tables needed to materialize a [`ResolvedFrame`].
struct DecodedMap {
    lines: Vec<Vec<Seg>>,
    sources: Vec<String>,
    names: Vec<String>,
}

/// A resolved anonymous frame: original source path, 1-based original line
/// for display, and the original function name when the mappings segment
/// carries a name index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedFrame {
    /// Original source path from `sources[src_idx]`.
    pub source: String,
    /// Original line, 1-based for display (`src_line + 1`).
    pub line: u32,
    /// Original function name from `names[name_idx]`, if the segment has one.
    pub name: Option<String>,
}

/// Resolution counters: scripts with ≥1 resolution, resolved frames total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceMapStats {
    pub scripts: usize,
    pub resolved_frames: usize,
}

struct StatsInner {
    resolved_frames: usize,
    resolved_urls: HashSet<String>,
}

/// Raw (undecoded) entry data owned by the resolver.
struct RawEntry {
    mappings: String,
    sources: Vec<String>,
    names: Vec<String>,
}

/// Lazy per-url source-map resolver. `Sync` so parallel CPU scans can share
/// one instance; decoded maps and stats hide behind locks. Anonymous frames
/// are rare, so lock contention on the hot path is negligible.
pub struct SourceMapResolver {
    raw: HashMap<String, RawEntry>,
    cache: RwLock<HashMap<String, Option<DecodedMap>>>,
    stats: Mutex<StatsInner>,
}

impl SourceMapResolver {
    /// Build over `entries` (url → entry, case-exact; first entry wins on
    /// duplicate urls). Only `mappings`/`names`/`sources` are retained —
    /// `sourcesContent` was already dropped at parse time.
    pub fn new(entries: &[SourceMapEntry]) -> Self {
        let mut raw: HashMap<String, RawEntry> = HashMap::with_capacity(entries.len());
        for e in entries {
            if e.url.is_empty() || raw.contains_key(&e.url) {
                continue;
            }
            raw.insert(
                e.url.clone(),
                RawEntry {
                    mappings: e.source_map.mappings.clone(),
                    sources: e.source_map.sources.clone(),
                    names: e.source_map.names.clone(),
                },
            );
        }
        Self {
            raw,
            cache: RwLock::new(HashMap::new()),
            stats: Mutex::new(StatsInner {
                resolved_frames: 0,
                resolved_urls: HashSet::new(),
            }),
        }
    }

    /// Number of urls with a source map.
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// True when no source maps were captured.
    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// Snapshot of `(scripts_resolved, frames_resolved)`.
    pub fn stats(&self) -> SourceMapStats {
        let s = self.stats.lock().unwrap();
        SourceMapStats {
            scripts: s.resolved_urls.len(),
            resolved_frames: s.resolved_frames,
        }
    }

    /// Resolve `(url, line0, col0)` — 0-based generated line/column from a
    /// `ProfileChunk` call frame — to the original source. Returns `None`
    /// on unknown url, out-of-range line, column before the first segment,
    /// unmapped segment, or a malformed map (marked bad forever).
    pub fn resolve(&self, url: &str, line0: u32, col0: u32) -> Option<ResolvedFrame> {
        self.ensure_decoded(url);
        // Query under a short read lock, cloning only the hit strings.
        let frame = {
            let cache = self.cache.read().unwrap();
            let decoded = cache.get(url)?.as_ref()?;
            let line = decoded.lines.get(line0 as usize)?;
            // Nearest segment with gen_col <= col0 (ascending order).
            let mut lo = 0usize;
            let mut hi = line.len();
            while lo < hi {
                let mid = (lo + hi) / 2;
                if line[mid].gen_col <= col0 {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo == 0 {
                return None;
            }
            let seg = &line[lo - 1];
            let src = seg.src.as_ref()?;
            let source = decoded.sources.get(src.src_idx as usize)?.clone();
            let name = src
                .name_idx
                .and_then(|i| decoded.names.get(i as usize).cloned());
            ResolvedFrame {
                source,
                line: src.src_line + 1,
                name,
            }
        };
        // Record the hit (scripts = distinct urls resolved).
        if let Ok(mut s) = self.stats.lock() {
            s.resolved_frames += 1;
            s.resolved_urls.insert(url.to_string());
        }
        Some(frame)
    }

    /// Decode `url` on first query and cache the result (including the
    /// bad-map marker). Unknown urls, a full cache, and malformed maps all
    /// resolve to `None` — never panics.
    fn ensure_decoded(&self, url: &str) {
        if self.cache.read().unwrap().contains_key(url) {
            return;
        }
        let mut cache = self.cache.write().unwrap();
        if cache.contains_key(url) {
            return;
        }
        if cache.len() >= MAX_DECODED {
            return;
        }
        let Some(raw) = self.raw.get(url) else {
            return;
        };
        let decoded = decode_map(&raw.mappings, &raw.sources, &raw.names);
        cache.insert(url.to_string(), decoded);
    }
}

/// Base64 digit → 6-bit value. Returns `None` on invalid chars.
fn b64(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a' + 26) as u32),
        b'0'..=b'9' => Some((c - b'0' + 52) as u32),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode one VLQ value from `bytes[pos..]`; advances `pos` past it.
fn decode_vlq(bytes: &[u8], pos: &mut usize) -> Option<i64> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= bytes.len() {
            return None;
        }
        let digit = b64(bytes[*pos])?;
        *pos += 1;
        let cont = digit & 32 != 0;
        let bits = digit & 31;
        if shift >= 30 && bits != 0 {
            // Overflow guard: deltas are small; anything larger is corrupt.
            return None;
        }
        result += bits << shift;
        shift += 5;
        if !cont {
            break;
        }
        if shift > 35 {
            return None;
        }
    }
    // Source-map VLQ: LSB is the sign, magnitude is `result >> 1` (NOT the
    // protobuf zigzag: digit 3 ('D') encodes -1, not -2; -0 encodes as 0).
    let v = result as i64;
    Some(if v & 1 != 0 { -(v >> 1) } else { v >> 1 })
}

/// Decode one `mappings` segment (comma-separated chunk) into its fields.
fn decode_segment(seg: &str) -> Option<Vec<i64>> {
    if seg.is_empty() {
        return None;
    }
    let bytes = seg.as_bytes();
    let mut pos = 0usize;
    let mut out = Vec::new();
    while pos < bytes.len() {
        out.push(decode_vlq(bytes, &mut pos)?);
        if out.len() > 5 {
            return None;
        }
    }
    match out.len() {
        1 | 4 | 5 => Some(out),
        _ => None,
    }
}

/// Decode a full `mappings` string into per-line segments. Deltas: `gen_col`
/// resets each line, all other fields carry across lines. Returns `None`
/// when any segment is malformed (the whole map is then marked bad).
fn decode_mappings(mappings: &str) -> Option<Vec<Vec<Seg>>> {
    let mut lines: Vec<Vec<Seg>> = Vec::new();
    let mut src_idx: i64 = 0;
    let mut src_line: i64 = 0;
    let mut src_col: i64 = 0;
    let mut name_idx: i64 = 0;
    // `str::split` on an empty string yields one empty item; a truly empty
    // mappings string is valid (zero lines with segments).
    if mappings.is_empty() {
        return Some(Vec::new());
    }
    for line_str in mappings.split(';') {
        let mut segs: Vec<Seg> = Vec::new();
        if !line_str.is_empty() {
            let mut gen_col: i64 = 0;
            for seg_str in line_str.split(',') {
                let fields = decode_segment(seg_str)?;
                gen_col += fields[0];
                if gen_col < 0 {
                    return None;
                }
                let src = if fields.len() >= 4 {
                    src_idx += fields[1];
                    src_line += fields[2];
                    src_col += fields[3];
                    let name = if fields.len() == 5 {
                        name_idx += fields[4];
                        if name_idx < 0 {
                            return None;
                        }
                        Some(name_idx as u32)
                    } else {
                        None
                    };
                    if src_idx < 0 || src_line < 0 || src_col < 0 {
                        return None;
                    }
                    Some(SegSrc {
                        src_idx: src_idx as u32,
                        src_line: src_line as u32,
                        name_idx: name,
                    })
                } else {
                    None
                };
                segs.push(Seg {
                    gen_col: gen_col as u32,
                    src,
                });
            }
        }
        lines.push(segs);
    }
    Some(lines)
}

/// Decode `mappings` with its `sources`/`names` tables into a [`DecodedMap`].
/// `None` when the mappings are malformed.
fn decode_map(mappings: &str, sources: &[String], names: &[String]) -> Option<DecodedMap> {
    let lines = decode_mappings(mappings)?;
    Some(DecodedMap {
        lines,
        sources: sources.to_vec(),
        names: names.to_vec(),
    })
}

/// Basename of a source path (`a/b/HUD.ts` → `HUD.ts`).
pub fn source_basename(source: &str) -> &str {
    source
        .rfind('/')
        .map(|i| &source[i + 1..])
        .unwrap_or(source)
}

/// Resolved display name: `"{display} ({basename}:{line})"` where `display`
/// is the original function name when the segment carries one, else the
/// source file's basename — e.g. `updateHUD (HUD.ts:142)`.
pub fn display_name(frame: &ResolvedFrame) -> String {
    let base = source_basename(&frame.source);
    let display = frame.name.as_deref().unwrap_or(base);
    format!("{display} ({base}:{line})", line = frame.line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode i64 values as VLQ base64 (test helper: inverse of decode).
    /// Source-map VLQ: sign bit is the LSB, magnitude is `|v| << 1`
    /// (`-1` → digit 3 'D', `+1` → digit 2 'C', `0`/`-0` → digit 0 'A').
    fn enc_vlq(v: i64) -> String {
        let mut uv: u32 = if v < 0 {
            (((-v) << 1) | 1) as u32
        } else {
            (v << 1) as u32
        };
        let mut out = String::new();
        const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        loop {
            let mut d = uv & 31;
            uv >>= 5;
            if uv != 0 {
                d |= 32;
            }
            out.push(B64[d as usize] as char);
            if uv == 0 {
                break;
            }
        }
        let _ = v;
        out
    }

    fn enc_seg(fields: &[i64]) -> String {
        fields.iter().map(|f| enc_vlq(*f)).collect()
    }

    #[test]
    fn vlq_roundtrip_signed() {
        for v in [-1000i64, -2, -1, 0, 1, 2, 1000, 123456] {
            let s = enc_vlq(v);
            let mut pos = 0;
            assert_eq!(decode_vlq(s.as_bytes(), &mut pos), Some(v), "v={v} s={s}");
            assert_eq!(pos, s.len());
        }
    }

    /// Spec pins (source-map VLQ, not protobuf zigzag): single-digit codes
    /// from the canonical table — 'A'=0, 'B'=-0(=0), 'C'=+1, 'D'=-1, 'E'=+2,
    /// 'F'=-2. Protobuf zigzag would decode 'D' as -2 and 'F' as -3.
    #[test]
    fn vlq_spec_single_digits() {
        for (s, v) in [("A", 0), ("B", 0), ("C", 1), ("D", -1), ("E", 2), ("F", -2)] {
            let mut pos = 0;
            assert_eq!(decode_vlq(s.as_bytes(), &mut pos), Some(v), "s={s}");
            assert_eq!(pos, 1);
        }
    }

    /// Hand-crafted map: two generated lines, negative deltas, name index.
    /// Line 0: col 0 → src0:10 name0; col 5 → src0:12 (delta -? col+5).
    /// Line 1: col 0 → src1:3 with negative srcLine delta.
    #[test]
    fn decode_multiline_negative_deltas_with_name() {
        // Line 0: [0,0,10,0,0] then [5,0,2,0] (srcLine 10→12).
        // Line 1: [0,1,-9,0,1] (src 0→1, line 12→3, name 0→1).
        let line0 = format!("{},{}", enc_seg(&[0, 0, 10, 0, 0]), enc_seg(&[5, 0, 2, 0]));
        let line1 = enc_seg(&[0, 1, -9, 0, 1]);
        let mappings = format!("{line0};{line1}");
        let lines = decode_mappings(&mappings).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 2);
        assert_eq!(lines[0][0].gen_col, 0);
        assert_eq!(
            lines[0][0].src,
            Some(SegSrc {
                src_idx: 0,
                src_line: 10,
                name_idx: Some(0)
            })
        );
        assert_eq!(lines[0][1].gen_col, 5);
        assert_eq!(lines[0][1].src.as_ref().unwrap().src_line, 12);
        assert_eq!(lines[0][1].src.as_ref().unwrap().name_idx, None);
        assert_eq!(lines[1].len(), 1);
        assert_eq!(
            lines[1][0].src,
            Some(SegSrc {
                src_idx: 1,
                src_line: 3,
                name_idx: Some(1)
            })
        );
    }

    #[test]
    fn resolve_hits_exact_source_line_name() {
        // Single line: col 0 → src0 line 141 (0-based) name0 "updateHUD".
        let mappings = enc_seg(&[0, 0, 141, 0, 0]);
        let entries = vec![SourceMapEntry {
            url: "http://localhost:5173/src/HUD.js".to_string(),
            source_map: super::super::meta::SourceMapData {
                mappings,
                names: vec!["updateHUD".to_string()],
                sources: vec!["src/HUD.ts".to_string()],
            },
        }];
        let r = SourceMapResolver::new(&entries);
        let f = r.resolve("http://localhost:5173/src/HUD.js", 0, 0).unwrap();
        assert_eq!(f.source, "src/HUD.ts");
        assert_eq!(f.line, 142);
        assert_eq!(f.name.as_deref(), Some("updateHUD"));
        assert_eq!(display_name(&f), "updateHUD (HUD.ts:142)");
        // Query col after the segment still hits it; stats recorded.
        assert!(r
            .resolve("http://localhost:5173/src/HUD.js", 0, 20)
            .is_some());
        let st = r.stats();
        assert_eq!(st.scripts, 1);
        assert_eq!(st.resolved_frames, 2);
    }

    #[test]
    fn resolve_boundary_queries_miss() {
        let mappings = format!("{},{}", enc_seg(&[4, 0, 0, 0]), enc_seg(&[6, 0, 1, 0]));
        let entries = vec![SourceMapEntry {
            url: "http://x/app.js".to_string(),
            source_map: super::super::meta::SourceMapData {
                mappings,
                names: vec![],
                sources: vec!["a.ts".to_string()],
            },
        }];
        let r = SourceMapResolver::new(&entries);
        // Column before the first segment.
        assert!(r.resolve("http://x/app.js", 0, 3).is_none());
        // Line out of range (only line 0 exists).
        assert!(r.resolve("http://x/app.js", 1, 0).is_none());
        // Unknown url.
        assert!(r.resolve("http://x/other.js", 0, 10).is_none());
        // Column between segments hits the first; after last hits the last.
        let f = r.resolve("http://x/app.js", 0, 5).unwrap();
        assert_eq!(f.line, 1);
        let f = r.resolve("http://x/app.js", 0, 100).unwrap();
        assert_eq!(f.line, 2);
    }

    #[test]
    fn malformed_map_is_bad_forever() {
        let entries = vec![SourceMapEntry {
            url: "http://x/bad.js".to_string(),
            source_map: super::super::meta::SourceMapData {
                mappings: "!!!not-base64!!!".to_string(),
                names: vec![],
                sources: vec!["a.ts".to_string()],
            },
        }];
        let r = SourceMapResolver::new(&entries);
        assert!(r.resolve("http://x/bad.js", 0, 0).is_none());
        // Second query hits the cached bad marker, still None, never panics.
        assert!(r.resolve("http://x/bad.js", 0, 0).is_none());
        assert_eq!(r.stats(), SourceMapStats::default());
    }

    #[test]
    fn unmapped_segment_misses() {
        // 1-field segment (genCol only) is unmapped.
        let mappings = enc_seg(&[3]);
        let entries = vec![SourceMapEntry {
            url: "http://x/u.js".to_string(),
            source_map: super::super::meta::SourceMapData {
                mappings,
                names: vec![],
                sources: vec!["a.ts".to_string()],
            },
        }];
        let r = SourceMapResolver::new(&entries);
        assert!(r.resolve("http://x/u.js", 0, 5).is_none());
    }

    #[test]
    fn trace_metadata_serde_skips_sources_content() {
        let json = serde_json::json!({
            "cpuThrottling": 4.0,
            "sourceMaps": [{
                "url": "http://x/app.js",
                "sourceMap": {
                    "version": 3,
                    "file": "app.js",
                    "mappings": "AAAA",
                    "names": ["f"],
                    "sources": ["a.ts"],
                    "sourcesContent": ["huge content that must not be retained"]
                }
            }]
        });
        let meta: super::super::meta::TraceMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(meta.source_maps.len(), 1);
        assert_eq!(meta.source_maps[0].url, "http://x/app.js");
        assert_eq!(meta.source_maps[0].source_map.mappings, "AAAA");
        assert_eq!(meta.source_maps[0].source_map.sources, vec!["a.ts"]);
        // Old traces without sourceMaps → empty vec.
        let old: super::super::meta::TraceMetadata =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(old.source_maps.is_empty());
    }
}
