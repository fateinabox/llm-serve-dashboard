//! sidecar — minimal control server for the LLM serve dashboard.
//!
//! Replaces sidecar.py. Provides:
//!   - /api/kv/*       KV cache save/restore/erase/swap/prune (proxies router /slots/0)
//!   - /api/models/*   Model load/unload (proxies router)
//!   - /              Sidecar dashboard HTML
//!   - /alt           Second design HTML
//!
//! Loopback-only, no auth (Tailscale handles network identity + TLS).

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

// ── Config (read at startup, stored in AppState) ───────────────────────────

fn env_port(key: &str, default: u16) -> u16 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn env_str(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[derive(Clone)]
struct Config {
    router_host: String,
    router_port: u16,
    kv_dir: String,
    kv_registry: String,
    kv_default_keep: u32,
    models_dir: String,
    presets_file: String,
}

impl Config {
    fn new() -> Self {
        let kv_dir = env_str("KV_SNAPSHOT_DIR", "/opt/models/kvcache");
        let models_dir = env_str("MODELS_DIR", "/opt/models");
        Self {
            router_host: "127.0.0.1".to_string(),
            router_port: env_port("LLAMA_ROUTER", 8080),
            kv_dir: kv_dir.clone(),
            kv_registry: format!("{}/kv-snapshots.json", kv_dir),
            kv_default_keep: env_port("KV_DEFAULT_KEEP", 3) as u32,
            models_dir: models_dir.clone(),
            presets_file: format!("{}/presets.ini", models_dir),
        }
    }
}

// ── Shared state ───────────────────────────────────────────────────────────

const METRIC_HOLD_MAX_S: f64 = 10.0;

#[derive(Clone)]
struct AppState {
    cfg: Config,
    client: reqwest::Client,
    /// Gauge hold: key → (value, last_live_time)
    metric_holds: Arc<Mutex<HashMap<String, (f64, f64)>>>,
}

impl AppState {
    fn new() -> Self {
        Self {
            cfg: Config::new(),
            client: reqwest::Client::new(),
            metric_holds: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn hold(&self, key: &str, value: f64, now: f64) -> (f64, Option<f64>) {
        let mut holds = self.metric_holds.lock().unwrap();
        let (held, last_live) = holds.get(key).copied().unwrap_or((0.0, 0.0));
        let (new_held, new_last) = if value > 0.0 {
            (value, now)
        } else if now - last_live > METRIC_HOLD_MAX_S {
            (0.0, 0.0)
        } else {
            (held, last_live)
        };
        holds.insert(key.to_string(), (new_held, new_last));
        let age = if new_last > 0.0 && new_held > 0.0 {
            Some(now - new_last)
        } else {
            None
        };
        (new_held, age)
    }
}

// ── Router helpers ─────────────────────────────────────────────────────────

fn router_url(cfg: &Config, path: &str, query: &str) -> String {
    let mut url = format!("http://{}:{}{}", cfg.router_host, cfg.router_port, path);
    if !query.is_empty() {
        url.push_str("?");
        url.push_str(query);
    }
    url
}

async fn router_get(state: &AppState, path: &str, query: &str) -> Result<Value, String> {
    let url = router_url(&state.cfg, path, query);
    state
        .client
        .get(&url)
        .header("User-Agent", "curl")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json::<Value>()
        .await
        .map_err(|e| e.to_string())
}

async fn router_post(state: &AppState, path: &str, body: &Value, query: &str) -> Value {
    let url = router_url(&state.cfg, path, query);
    let resp = match state
        .client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("User-Agent", "curl")
        .json(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return json!({"error": e.to_string()}),
    };
    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return json!({"error": e.to_string()}),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => v,
        Err(_) => {
            if status.is_success() {
                json!({"raw": text})
            } else {
                json!({"error": text})
            }
        }
    }
}

async fn loaded_model(state: &AppState) -> Option<String> {
    let d = router_get(state, "/v1/models", "").await.ok()?;
    let data = d.get("data")?.as_array()?;
    for m in data {
        let id = m.get("id")?.as_str()?;
        if id.is_empty() || id == ".noindex" {
            continue;
        }
        let status = m.get("status")?;
        if status.get("value").and_then(|v| v.as_str()) == Some("loaded") {
            return Some(id.to_string());
        }
    }
    None
}

// ── Worker metrics ─────────────────────────────────────────────────────────

async fn worker_port(state: &AppState) -> Option<u16> {
    let d = router_get(state, "/v1/models", "").await.ok()?;
    let data = d.get("data")?.as_array()?;
    for m in data {
        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id == ".noindex" {
            continue;
        }
        let status = m.get("status");
        if status.map(|s| s.get("value").and_then(|v| v.as_str())) != Some(Some("loaded")) {
            continue;
        }
        let args = status.and_then(|s| s.get("args")).and_then(|a| a.as_array());
        if let Some(args) = args {
            for i in 0..args.len().saturating_sub(1) {
                if args[i].as_str() == Some("--port") {
                    if let Some(port) = args[i + 1].as_str().and_then(|p| p.parse::<u16>().ok()) {
                        if port > 0 {
                            return Some(port);
                        }
                    }
                }
            }
        }
    }
    None
}

async fn http_json(port: u16, path: &str) -> Value {
    let url = format!("http://127.0.0.1:{}{}", port, path);
    let req = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "curl")
        .timeout(std::time::Duration::from_secs(5));
    match req.send().await {
        Ok(r) => r.json::<Value>().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

async fn http_text(port: u16, path: &str) -> String {
    let url = format!("http://127.0.0.1:{}{}", port, path);
    let req = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "curl")
        .timeout(std::time::Duration::from_secs(5));
    match req.send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => String::new(),
    }
}

fn parse_metrics(text: &str) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    for line in text.split('\n') {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let name = parts[0];
        if name.contains('{') {
            continue;
        }
        if let Ok(v) = parts[1].parse::<f64>() {
            out.insert(name.to_string(), v);
        }
    }
    out
}

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

async fn worker_metrics(state: &AppState) -> Value {
    let port = match worker_port(state).await {
        Some(p) => p,
        None => return json!({}),
    };

    let metrics = parse_metrics(&http_text(port, "/metrics").await);
    let props = http_json(port, "/props").await;
    if metrics.is_empty() && props.is_null() {
        return json!({});
    }

    let n_ctx = props
        .get("default_generation_settings")
        .and_then(|d| d.get("n_ctx"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let n_tokens_max = metrics.get("llamacpp:n_tokens_max").copied().unwrap_or(0.0);
    let kv_ratio = metrics.get("llamacpp:kv_cache_usage_ratio").copied().unwrap_or(0.0);
    let kv_ratio = if kv_ratio == 0.0 && n_tokens_max > 0.0 && n_ctx > 0 {
        n_tokens_max / n_ctx as f64
    } else {
        kv_ratio
    };

    let now = now_f64();
    let decode_tps_raw = metrics.get("llamacpp:predicted_tokens_seconds").copied().unwrap_or(0.0);
    let (decode_tps, decode_age) = state.hold("tg", decode_tps_raw, now);
    let prompt_tps_raw = metrics.get("llamacpp:prompt_tokens_seconds").copied().unwrap_or(0.0);
    let (prompt_tps, prompt_age) = state.hold("pp", prompt_tps_raw, now);

    let spec_acc = metrics
        .get("llamacpp:spec_decode_num_accepted_tokens_total")
        .copied()
        .unwrap_or(0.0);
    let spec_draft = metrics
        .get("llamacpp:spec_decode_num_draft_tokens_total")
        .copied()
        .unwrap_or(0.0);

    // Prompt-ingestion progress from router /slots
    let (mut prompt_processed, mut prompt_total, mut slot_prompt_tokens) = (0u64, 0u64, 0u64);
    if let Some(model) = loaded_model(state).await {
        let query = format!("model={}", urlencoded(&model));
        if let Ok(d) = router_get(state, "/slots", &query).await {
            let slots = if d.is_array() {
                d.clone()
            } else {
                d.get("slots").cloned().unwrap_or(Value::Null)
            };
            if let Some(slots) = slots.as_array() {
                if let Some(s0) = slots.first() {
                    prompt_processed = s0.get("n_prompt_tokens_processed").and_then(|v| v.as_u64()).unwrap_or(0);
                    prompt_total = s0.get("n_prompt_tokens_total").and_then(|v| v.as_u64()).unwrap_or(0);
                    slot_prompt_tokens = s0.get("n_prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
        }
    }

    // Context fill
    let (ctx_used, ctx_pct) = if slot_prompt_tokens > 0 && n_ctx > 0 {
        (
            slot_prompt_tokens,
            (100.0 * slot_prompt_tokens as f64 / n_ctx as f64 * 10.0).round() / 10.0,
        )
    } else if kv_ratio > 0.0 && n_ctx > 0 {
        (
            (kv_ratio * n_ctx as f64) as u64,
            (kv_ratio * 100.0 * 10.0).round() / 10.0,
        )
    } else {
        (0, 0.0)
    };

    let spec_accept_pct = if spec_draft > 0.0 {
        Some((100.0 * spec_acc / spec_draft * 10.0).round() / 10.0)
    } else {
        None
    };

    let prompt_ingest_pct = if prompt_processed > 0 || prompt_total > 0 {
        (100.0 * prompt_processed as f64 / (prompt_total.max(prompt_processed).max(1)) as f64 * 10.0)
            .round()
            / 10.0
    } else {
        0.0
    };

    json!({
        "port": port,
        "n_ctx": n_ctx,
        "ctx_used_tokens": ctx_used,
        "ctx_fill_pct": ctx_pct,
        "decode_tps": (decode_tps * 10.0).round() / 10.0,
        "decode_tps_age_s": decode_age.map(|a| (a * 10.0).round() / 10.0),
        "prompt_tps": (prompt_tps * 10.0).round() / 10.0,
        "prompt_tps_age_s": prompt_age.map(|a| (a * 10.0).round() / 10.0),
        "spec_accept_pct": spec_accept_pct,
        "prompt_ingest_pct": prompt_ingest_pct,
        "prompt_processed": prompt_processed,
        "prompt_total": prompt_total,
        "requests_processing": metrics.get("llamacpp:requests_processing").copied().unwrap_or(0.0),
        "requests_waiting": metrics.get("llamacpp:requests_deferred").copied().unwrap_or(0.0),
        "busy_slots_per_decode": metrics.get("llamacpp:n_busy_slots_per_decode").cloned(),
    })
}

fn urlencoded(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => "+".to_string(),
            c => {
                c.to_string()
                    .as_bytes()
                    .iter()
                    .map(|b| format!("%{:02X}", b))
                    .collect()
            }
        })
        .collect()
}

// ── KV registry ────────────────────────────────────────────────────────────

fn kv_read_registry(cfg: &Config) -> Value {
    fs::read_to_string(&cfg.kv_registry)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json!({}))
}

fn kv_write_registry(cfg: &Config, reg: &Value) {
    if let Some(parent) = Path::new(&cfg.kv_registry).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(
        &cfg.kv_registry,
        serde_json::to_string_pretty(reg).unwrap_or_default(),
    );
}

fn kv_tag(cfg: &Config, filename: &str, model: &str) {
    let mut reg = kv_read_registry(cfg);
    let path = format!("{}/{}", cfg.kv_dir, filename);
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mtime = fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Some(obj) = reg.as_object_mut() {
        obj.insert(
            filename.to_string(),
            json!({"model": model, "size": size, "mtime": mtime}),
        );
    }
    kv_write_registry(cfg, &reg);
}

fn kv_list_snapshots(cfg: &Config, model: Option<&str>) -> Vec<Value> {
    let reg = kv_read_registry(cfg);
    let mut snaps: Vec<Value> = Vec::new();
    if let Ok(entries) = fs::read_dir(&cfg.kv_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "kv-snapshots.json" {
                continue;
            }
            if !entry.path().is_file() {
                continue;
            }
            let entry_val = reg.get(&name).cloned().unwrap_or(json!({}));
            let snap_model = entry_val.get("model").and_then(|v| v.as_str());
            if let Some(m) = model {
                if snap_model != Some(m) {
                    continue;
                }
            }
            let path = format!("{}/{}", cfg.kv_dir, name);
            let size = entry_val
                .get("size")
                .and_then(|v| v.as_u64())
                .or_else(|| fs::metadata(&path).map(|m| m.len()).ok())
                .unwrap_or(0);
            let mtime = entry_val
                .get("mtime")
                .and_then(|v| v.as_f64())
                .or_else(|| {
                    fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs_f64())
                })
                .unwrap_or(0.0);
            snaps.push(json!({"filename": name, "model": snap_model, "size": size, "mtime": mtime}));
        }
    }
    snaps.sort_by(|a, b| {
        let ma = a.get("mtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let mb = b.get("mtime").and_then(|v| v.as_f64()).unwrap_or(0.0);
        mb.partial_cmp(&ma).unwrap_or(std::cmp::Ordering::Equal)
    });
    snaps
}

fn kv_prune(cfg: &Config, keep: u32, model: Option<&str>) -> Value {
    let snaps = kv_list_snapshots(cfg, model);
    if snaps.len() <= keep as usize {
        return json!({"deleted": 0, "kept": snaps.len()});
    }
    let to_delete: Vec<String> = snaps[keep as usize..]
        .iter()
        .map(|s| s["filename"].as_str().unwrap_or("").to_string())
        .collect();
    let mut deleted = 0u32;
    let mut reg = kv_read_registry(cfg);
    for name in &to_delete {
        let path = format!("{}/{}", cfg.kv_dir, name);
        if fs::metadata(&path).is_ok() {
            let _ = fs::remove_file(&path);
            deleted += 1;
        }
        if let Some(obj) = reg.as_object_mut() {
            obj.remove(name);
        }
    }
    kv_write_registry(cfg, &reg);
    json!({"deleted": deleted, "kept": keep})
}

async fn kv_router_call(state: &AppState, action: &str, payload: &Value) -> Value {
    router_post(state, &format!("/slots/0?action={}", action), payload, "").await
}

// ── Model roster ───────────────────────────────────────────────────────────

fn parse_presets(cfg: &Config) -> Vec<Value> {
    let mut presets: Vec<Value> = Vec::new();
    let content = match fs::read_to_string(&cfg.presets_file) {
        Ok(c) => c,
        Err(_) => return presets,
    };
    let mut current: Option<String> = None;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = &line[1..line.len() - 1];
            current = Some(name.to_string());
            if name != "*" {
                presets.push(json!({"name": name, "model": name}));
            }
            continue;
        }
        if let Some(cur) = &current {
            if let Some(eq) = line.find('=') {
                let key = line[..eq].trim();
                let val = line[eq + 1..].trim();
                if key == "model" {
                    for p in &mut presets {
                        if p["name"].as_str() == Some(cur.as_str()) {
                            if let Some(obj) = p.as_object_mut() {
                                obj.insert("model".to_string(), json!(val));
                            }
                        }
                    }
                }
            }
        }
    }
    presets
}

fn scan_models_dir(cfg: &Config) -> Vec<String> {
    let preset_models: Vec<String> = parse_presets(cfg)
        .iter()
        .filter_map(|p| {
            p["model"].as_str().map(|m| {
                m.split('/').last().unwrap_or(m).to_string()
            })
        })
        .collect();
    let mut others: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(&cfg.models_dir) {
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".gguf"))
            .collect();
        names.sort();
        for name in names {
            if !preset_models.contains(&name) {
                others.push(name);
            }
        }
    }
    others
}

// ── GPU detection ──────────────────────────────────────────────────────────

fn detect_gpu() -> Value {
    let mut vendor = String::from("unknown");
    let mut name = String::from("unknown");

    if let Ok(out) = Command::new("lspci").output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if line.contains("VGA") || line.contains("3D") || line.contains("Display") {
                let low = line.to_lowercase();
                if low.contains("nvidia") {
                    vendor = String::from("nvidia");
                } else if low.contains("amd") || low.contains("ati") {
                    vendor = String::from("amd");
                } else if low.contains("intel") {
                    vendor = String::from("intel");
                }
                if let Some(rest) = line.split_once(':') {
                    name = rest.1.trim().to_string();
                } else {
                    name = line.to_string();
                }
                break;
            }
        }
    }

    let mut metrics: Value = json!({});

    if vendor == "amd" {
        if let Ok(out) = Command::new("/usr/bin/amdgpu_top").args(["-d", "-J"]).output() {
            if let Ok(data) = serde_json::from_slice::<Value>(&out.stdout) {
                if let Some(arr) = data.as_array() {
                    for d in arr {
                        if d.get("GPU Type").and_then(|v| v.as_str()) == Some("dGPU") {
                            let sensors = d.get("Sensors").cloned().unwrap_or(json!({}));
                            let vram = d.get("VRAM").cloned().unwrap_or(json!({}));
                            let activity = d.get("gpu_activity").cloned().unwrap_or(json!({}));
                            let sv = |obj: &Value, key: &str| -> f64 {
                                obj.get(key)
                                    .and_then(|v| v.get("value"))
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0)
                            };
                            metrics = json!({
                                "power_w": sv(&sensors, "Average Power"),
                                "temp_c": sv(&sensors, "Edge Temperature"),
                                "freq_mhz": sv(&sensors, "GFX_SCLK"),
                                "vram_used_mib": sv(&vram, "Total VRAM Usage"),
                                "vram_total_mib": sv(&vram, "Total VRAM"),
                                "gfx_percent": sv(&activity, "GFX"),
                                "fan_rpm": sv(&sensors, "Fan"),
                            });
                            break;
                        }
                    }
                }
            }
        }
        if metrics.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            for card in ["card0", "card1"] {
                let hwmon_dir = format!("/sys/class/drm/{}/device/hwmon", card);
                if let Ok(entries) = fs::read_dir(&hwmon_dir) {
                    for entry in entries.flatten() {
                        let hwmon = entry.path();
                        let rd = |f: &str| -> Option<u64> {
                            fs::read_to_string(hwmon.join(f))
                                .ok()
                                .and_then(|s| s.trim().parse::<u64>().ok())
                        };
                        if rd("power1_input").is_some() {
                            let p = rd("power1_input").unwrap_or(0);
                            let t = rd("temp1_input").unwrap_or(0);
                            let fr = rd("freq1_input").unwrap_or(0);
                            metrics = json!({
                                "power_w": (p as f64) / 1e6,
                                "temp_c": (t as f64) / 1000.0,
                                "freq_mhz": (fr as f64) / 1e6,
                            });
                            break;
                        }
                    }
                    if !metrics.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                        break;
                    }
                }
            }
        }
    } else if vendor == "nvidia" {
        if let Ok(out) = Command::new("nvidia-smi")
            .args([
                "--query-gpu=power.draw,temperature.gpu,clocks.current.graphics,utilization.gpu,memory.used,memory.total",
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let parts: Vec<&str> = stdout.trim().split(',').collect();
            if parts.len() >= 6 {
                let parse = |s: &str| s.trim().parse::<f64>().unwrap_or(0.0);
                metrics = json!({
                    "power_w": parse(parts[0]),
                    "temp_c": parse(parts[1]),
                    "freq_mhz": parse(parts[2]),
                    "gfx_percent": parse(parts[3]),
                    "vram_used_mib": parse(parts[4]),
                    "vram_total_mib": parse(parts[5]),
                });
            }
        }
    }

    let mut result = json!({"vendor": vendor, "name": name});
    if let (Some(r), Some(m)) = (result.as_object_mut(), metrics.as_object()) {
        for (k, v) in m {
            r.insert(k.clone(), v.clone());
        }
    }
    result
}

// ── System metrics ─────────────────────────────────────────────────────────

fn system_metrics() -> Value {
    let sys = sysinfo::System::new_all();
    let mem_total = sys.total_memory() as f64 / (1024.0 * 1024.0);
    let mem_used = sys.used_memory() as f64 / (1024.0 * 1024.0);
    let cpu_pct = sys.global_cpu_usage();
    let cpu_count = sys.cpus().len() as u32;

    // Parse `df` for root disk usage
    let (mut disk_used, mut disk_total) = (0u64, 0u64);
    if let Ok(out) = Command::new("df").args(["-B1", "/"]).output() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = stdout.lines().nth(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 4 {
                disk_total = parts[1].parse().unwrap_or(0);
                disk_used = parts[2].parse().unwrap_or(0);
            }
        }
    }

    let load: Vec<f64> = fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| {
            s.split_whitespace()
                .take(3)
                .map(|x| x.parse::<f64>().ok())
                .collect()
        })
        .unwrap_or_else(|| vec![0.0, 0.0, 0.0]);

    let mut temps = serde_json::Map::new();
    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if let Ok(sensors) = fs::read_dir(&dir) {
                for sensor in sensors.flatten() {
                    let sensor_name = sensor.file_name().to_string_lossy().to_string();
                    if sensor_name.starts_with("temp") && sensor_name.ends_with("_input") {
                        if let Ok(val) = fs::read_to_string(sensor.path()) {
                            let raw: f64 = val.trim().parse().unwrap_or(0.0);
                            // hwmon temp*_input is in millidegrees
                            let v = raw / 1000.0;
                            let key = sensor_name
                                .trim_end_matches("_input")
                                .trim_start_matches("temp");
                            let key = if key.is_empty() { name.clone() } else { key.to_string() };
                            if !temps.contains_key(&key) {
                                temps.insert(key, json!((v * 10.0).round() / 10.0));
                            }
                        }
                    }
                }
            }
        }
    }

    json!({
        "ram_used_mb": (mem_used * 10.0).round() / 10.0,
        "ram_total_mb": (mem_total * 10.0).round() / 10.0,
        "ram_used_pct": if mem_total > 0.0 {
            (mem_used / mem_total * 100.0 * 10.0).round() / 10.0
        } else {
            0.0
        },
        "cpu_percent": cpu_pct,
        "cpu_count": cpu_count,
        "load_1m": (load.first().copied().unwrap_or(0.0) * 100.0).round() / 100.0,
        "load_5m": (load.get(1).copied().unwrap_or(0.0) * 100.0).round() / 100.0,
        "load_15m": (load.get(2).copied().unwrap_or(0.0) * 100.0).round() / 100.0,
        "disk_used_gb": (disk_used as f64 / (1024.0 * 1024.0 * 1024.0) * 10.0).round() / 10.0,
        "disk_total_gb": (disk_total as f64 / (1024.0 * 1024.0 * 1024.0) * 10.0).round() / 10.0,
        "disk_used_pct": if disk_total > 0 {
            (100.0 * disk_used as f64 / disk_total as f64 * 10.0).round() / 10.0
        } else {
            0.0
        },
        "temps": Value::Object(temps),
    })
}

// ── Log tail ───────────────────────────────────────────────────────────────

fn active_llama_server_unit() -> Option<String> {
    let out = Command::new("systemctl")
        .args(["list-units", "llama-server@*.service", "--state=active", "--no-legend"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let Some(first) = parts.first() {
            if first.ends_with(".service") {
                return Some(first.to_string());
            }
        }
    }
    None
}

fn log_tail() -> Value {
    let unit = match active_llama_server_unit() {
        Some(u) => u,
        None => return json!({"error": "no active llama-server@*.service instance"}),
    };

    let mut r = Command::new("journalctl")
        .args(["-u", &unit, "--no-pager", "-n", "200"])
        .output();

    if r
        .as_ref()
        .map(|o| o.status.code() != Some(0) || o.stdout.is_empty())
        .unwrap_or(true)
    {
        r = Command::new("sudo")
            .args(["-n", "/usr/bin/journalctl", "-u", &unit, "--no-pager", "-n", "200"])
            .output();
    }

    match r {
        Ok(output) => {
            if output.status.success() {
                let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|l| l.to_string())
                    .collect();
                json!({"lines": lines, "total": lines.len()})
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                json!({"error": stderr.trim().to_string()})
            }
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}

// ── HTML serving ───────────────────────────────────────────────────────────

fn serve_html(path: &str) -> axum::response::Response {
    match fs::read(path) {
        Ok(body) => {
            let mtime = fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let size = body.len() as u64;
            let token = format!("{}-{}", mtime, size);
            let html = String::from_utf8_lossy(&body).to_string();
            let html = html.replacen("<head>", &format!("<head><!--cb:{}-->", token), 1);
            (StatusCode::OK, html).into_response()
        }
        Err(_) => Json(json!({"error": format!("{} not found", path)})).into_response(),
    }
}

// ── Timestamp helper ───────────────────────────────────────────────────────

fn chrono_now() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs = now.as_secs();
    let days = secs / 86400;
    let rem = secs % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let y = 1970 + (days as i64) / 365;
    let mo = ((days % 365) / 30 + 1) as u32;
    let d = (days % 30 + 1) as u32;
    format!("{:04}{:02}{:02}-{:02}{:02}{:02}", y, mo, d, h, m, s)
}

// ── Routes ─────────────────────────────────────────────────────────────────

fn base_dir() -> String {
    // Use the directory containing the executable, falling back to CWD.
    // In practice the binary is at <project>/target/debug/sidecar,
    // so we look for sidecar.html in CWD first, then next to the binary.
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    if cwd.join("sidecar.html").exists() {
        return cwd.to_string_lossy().to_string();
    }
    // Fall back: binary's grandparent (target/debug → project root)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            if root.join("sidecar.html").exists() {
                return root.to_string_lossy().to_string();
            }
        }
    }
    cwd.to_string_lossy().to_string()
}

async fn get_root(State(_state): State<AppState>) -> axum::response::Response {
    let path = format!("{}/sidecar.html", base_dir());
    serve_html(&path)
}

async fn get_alt(State(_state): State<AppState>) -> axum::response::Response {
    let path = format!("{}/alt.html", base_dir());
    serve_html(&path)
}

async fn get_health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn get_gpu() -> Json<Value> {
    Json(detect_gpu())
}

async fn get_system() -> Json<Value> {
    Json(system_metrics())
}

async fn get_log() -> Json<Value> {
    Json(log_tail())
}

async fn get_metrics(State(state): State<AppState>) -> Json<Value> {
    Json(worker_metrics(&state).await)
}

async fn get_slots(State(state): State<AppState>) -> axum::response::Response {
    let model = loaded_model(&state).await;
    match model {
        None => Json(json!([])).into_response(),
        Some(m) => {
            let query = format!("model={}", urlencoded(&m));
            match router_get(&state, "/slots", &query).await {
                Ok(d) => {
                    let slots = if d.is_array() {
                        d
                    } else {
                        d.get("slots").cloned().unwrap_or(Value::Null)
                    };
                    let out: Vec<Value> = slots
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|s| {
                                    json!({
                                        "id": s.get("id").cloned(),
                                        "is_processing": s.get("is_processing").cloned().unwrap_or(json!(false)),
                                        "n_prompt_tokens": s.get("n_prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                                        "n_prompt_tokens_processed": s.get("n_prompt_tokens_processed").and_then(|v| v.as_u64()).unwrap_or(0),
                                        "n_prompt_tokens_cache": s.get("n_prompt_tokens_cache").and_then(|v| v.as_u64()).unwrap_or(0),
                                        "n_prompt_tokens_total": s.get("n_prompt_tokens_total").and_then(|v| v.as_u64()).unwrap_or(0),
                                        "decode_tokens_seconds": s.get("decode_tokens_seconds").and_then(|v| v.as_f64()).unwrap_or(0.0),
                                        "prompt_tokens_seconds": s.get("prompt_tokens_seconds").and_then(|v| v.as_f64()).unwrap_or(0.0),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Json(Value::Array(out)).into_response()
                }
                Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
            }
        }
    }
}

async fn get_roster(State(state): State<AppState>) -> Json<Value> {
    let model = loaded_model(&state).await;
    Json(json!({
        "presets": parse_presets(&state.cfg),
        "others": scan_models_dir(&state.cfg),
        "model": model,
    }))
}

async fn get_kv_snapshots(State(state): State<AppState>) -> Json<Value> {
    let model = loaded_model(&state).await;
    let snaps = kv_list_snapshots(&state.cfg, model.as_deref());
    Json(json!({
        "snapshots": snaps,
        "model": model,
        "dir": state.cfg.kv_dir,
    }))
}

async fn post_kv_save(State(state): State<AppState>, Json(payload): Json<Value>) -> Json<Value> {
    let model = loaded_model(&state).await;
    let filename = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| chrono_now());
    let result = kv_router_call(&state, "save", &json!({"model": model, "filename": filename})).await;
    if result.get("error").is_none() {
        kv_tag(&state.cfg, &filename, model.as_deref().unwrap_or("unknown"));
    }
    Json(result)
}

async fn post_kv_restore(State(state): State<AppState>, Json(payload): Json<Value>) -> Json<Value> {
    let model = loaded_model(&state).await;
    let filename = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            let snaps = kv_list_snapshots(&state.cfg, model.as_deref());
            snaps.first().and_then(|s| s["filename"].as_str().map(|f| f.to_string()))
        });
    match filename {
        Some(f) => {
            let result = kv_router_call(&state, "restore", &json!({"model": model, "filename": f})).await;
            Json(result)
        }
        None => Json(json!({"error": "no snapshots for loaded model"})),
    }
}

async fn post_kv_erase(State(state): State<AppState>) -> Json<Value> {
    let model = loaded_model(&state).await;
    let result = kv_router_call(&state, "erase", &json!({"model": model})).await;
    Json(result)
}

async fn post_kv_swap(State(state): State<AppState>, Json(payload): Json<Value>) -> Json<Value> {
    let model = loaded_model(&state).await;
    let filename = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| chrono_now());
    let saved = kv_router_call(&state, "save", &json!({"model": model, "filename": filename})).await;
    if saved.get("error").is_some() {
        return Json(saved);
    }
    kv_tag(&state.cfg, &filename, model.as_deref().unwrap_or("unknown"));
    let erased = kv_router_call(&state, "erase", &json!({"model": model})).await;
    Json(json!({"saved": saved, "erased": erased}))
}

async fn post_kv_prune(State(state): State<AppState>, Json(payload): Json<Value>) -> Json<Value> {
    let model = loaded_model(&state).await;
    let keep = payload
        .get("keep")
        .and_then(|v| v.as_u64())
        .unwrap_or(state.cfg.kv_default_keep as u64) as u32;
    Json(kv_prune(&state.cfg, keep, model.as_deref()))
}

async fn post_models_load(State(state): State<AppState>, Json(payload): Json<Value>) -> Json<Value> {
    let model_id = payload.get("model").and_then(|v| v.as_str());
    match model_id {
        Some(id) => {
            let result = router_post(&state, "/v1/models/load", &json!({"model": id}), "").await;
            Json(result)
        }
        None => Json(json!({"error": "model required"})),
    }
}

async fn post_models_unload(State(state): State<AppState>, Json(payload): Json<Value>) -> Json<Value> {
    let model_id = payload.get("model").and_then(|v| v.as_str());
    match model_id {
        Some(id) => {
            let result = router_post(&state, "/v1/models/unload", &json!({"model": id}), "").await;
            Json(result)
        }
        None => Json(json!({"error": "model required"})),
    }
}

// ── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let state = AppState::new();
    let port: u16 = std::env::var("SIDECAR_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8092);

    println!("sidecar listening on 127.0.0.1:{}", port);
    println!("router: {}:{}" , state.cfg.router_host, state.cfg.router_port);
    println!("kv dir: {}", state.cfg.kv_dir);

    let app = Router::new()
        .route("/", get(get_root))
        .route("/alt", get(get_alt))
        .route("/health", get(get_health))
        .route("/api/gpu", get(get_gpu))
        .route("/api/system", get(get_system))
        .route("/api/log", get(get_log))
        .route("/api/metrics", get(get_metrics))
        .route("/api/slots", get(get_slots))
        .route("/api/models/roster", get(get_roster))
        .route("/api/kv/snapshots", get(get_kv_snapshots))
        .route("/api/kv/save", post(post_kv_save))
        .route("/api/kv/restore", post(post_kv_restore))
        .route("/api/kv/erase", post(post_kv_erase))
        .route("/api/kv/swap", post(post_kv_swap))
        .route("/api/kv/prune", post(post_kv_prune))
        .route("/api/models/load", post(post_models_load))
        .route("/api/models/unload", post(post_models_unload))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("failed to bind");

    axum::serve(listener, app)
        .await
        .expect("server error");
}
