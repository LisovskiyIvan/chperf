use serde::Deserialize;
use std::io::BufReader;
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

#[derive(Deserialize, Clone)]
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
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

pub fn parse_trace(path: &Path) -> Result<TraceFile, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    // Stream .gz through a gzip decoder so serde_json reads decompressed bytes
    // without buffering the whole file in memory.
    let trace: TraceFile = if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        serde_json::from_reader(flate2::read::GzDecoder::new(reader))?
    } else {
        serde_json::from_reader(reader)?
    };
    let mut trace = trace;

    // Extract page URL from TracingStartedInBrowser event
    {
        let meta = trace.metadata.get_or_insert_with(|| TraceMetadata {
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
                    if let Some(ref args) = e.args {
                        if let Some(frames) = args
                            .get("data")
                            .and_then(|d| d.get("frames"))
                            .and_then(|f| f.as_array())
                        {
                            for frame in frames {
                                if let Some(url) = frame.get("url").and_then(|u| u.as_str()) {
                                    if !url.is_empty() && url != "about:blank" {
                                        meta.page_url = Some(url.to_string());
                                        break;
                                    }
                                }
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

/// Detect main thread: first RunTask with dur > 500ms
pub fn detect_main_thread(events: &[TraceEvent]) -> u64 {
    for e in events {
        if e.name == "RunTask" && e.ph == "X" {
            if let Some(dur) = e.dur {
                if dur > 500_000.0 {
                    return e.tid;
                }
            }
        }
    }
    // Fallback: tid with most RunTask events
    let mut counts: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
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
