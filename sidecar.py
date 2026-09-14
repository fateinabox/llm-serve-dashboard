#!/usr/bin/env python3
"""
sidecar.py — minimal control server for the LLM serve dashboard.

Replaces fleet-metrics.py. Provides:
  - /api/kv/*       KV cache save/restore/erase/swap/prune (proxies router /slots/0)
  - /api/models/*   Model load/unload (proxies router)
  - /              Sidecar dashboard HTML
  - /alt           Second design HTML

Loopback-only, stdlib-only, no deps.
"""
import json
import os
import re
import time
import urllib.request
import urllib.error
import http.server
import socket

# --- Config ---
ROUTER_PORT = int(os.environ.get("LLAMA_ROUTER", "8080"))
ROUTER_HOST = "127.0.0.1"
KV_DIR = os.environ.get("KV_SNAPSHOT_DIR", "/opt/models/kvcache")
KV_REGISTRY = os.path.join(KV_DIR, "kv-snapshots.json")
KV_DEFAULT_KEEP = int(os.environ.get("KV_DEFAULT_KEEP", "3"))
MODELS_DIR = os.environ.get("MODELS_DIR", "/opt/models")
PRESETS_FILE = os.path.join(MODELS_DIR, "presets.ini")

# --- Router helpers ---

def router_url(path, query=""):
    base = f"http://{ROUTER_HOST}:{ROUTER_PORT}"
    url = f"{base}{path}"
    if query:
        url += f"?{query}"
    return url


def router_get(path, query=""):
    req = urllib.request.Request(router_url(path, query), headers={"User-Agent": "curl"})
    resp = urllib.request.urlopen(req, timeout=10)
    return json.loads(resp.read().decode("utf-8", errors="replace"))


def router_post(path, body=None, query=""):
    data = json.dumps(body or {}).encode("utf-8")
    req = urllib.request.Request(
        router_url(path, query),
        data=data,
        headers={"Content-Type": "application/json", "User-Agent": "curl"},
        method="POST",
    )
    try:
        resp = urllib.request.urlopen(req, timeout=30)
        return json.loads(resp.read().decode("utf-8", errors="replace"))
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            return json.loads(raw)
        except Exception:
            return {"error": raw or str(e)}


def loaded_model():
    """Return the loaded model id from /v1/models, or None."""
    try:
        d = router_get("/v1/models")
        for m in d.get("data", []):
            if m.get("id") and m.get("id") != ".noindex":
                status = m.get("status", {})
                if isinstance(status, dict) and status.get("value") == "loaded":
                    return m["id"]
    except Exception:
        pass
    return None


# --- KV registry ---

def _kv_read_registry():
    if not os.path.exists(KV_REGISTRY):
        return {}
    try:
        with open(KV_REGISTRY, "r") as f:
            return json.load(f)
    except Exception:
        return {}


def _kv_write_registry(reg):
    with open(KV_REGISTRY, "w") as f:
        json.dump(reg, f, indent=2)


def _kv_tag(filename, model):
    """Tag a snapshot filename with its model in the registry."""
    reg = _kv_read_registry()
    reg[filename] = {
        "model": model,
        "size": os.path.getsize(os.path.join(KV_DIR, filename)) if os.path.exists(os.path.join(KV_DIR, filename)) else 0,
        "mtime": time.time(),
    }
    _kv_write_registry(reg)


def kv_list_snapshots(model=None):
    """List snapshots, optionally filtered by model."""
    snaps = []
    reg = _kv_read_registry()
    if os.path.isdir(KV_DIR):
        for name in os.listdir(KV_DIR):
            if name == "kv-snapshots.json":
                continue
            if not os.path.isfile(os.path.join(KV_DIR, name)):
                continue
            entry = reg.get(name, {})
            snap_model = entry.get("model")
            if model and snap_model != model:
                continue
            snaps.append({
                "filename": name,
                "model": snap_model,
                "size": entry.get("size", os.path.getsize(os.path.join(KV_DIR, name)) if os.path.exists(os.path.join(KV_DIR, name)) else 0),
                "mtime": entry.get("mtime", os.path.getmtime(os.path.join(KV_DIR, name)) if os.path.exists(os.path.join(KV_DIR, name)) else 0),
            })
    snaps.sort(key=lambda s: s["mtime"], reverse=True)
    return snaps


def kv_prune(keep, model=None):
    """Keep the N most recent snapshots for the loaded model, delete the rest."""
    snaps = kv_list_snapshots(model)
    if len(snaps) <= keep:
        return {"deleted": 0, "kept": len(snaps)}
    to_delete = [s["filename"] for s in snaps[keep:]]
    deleted = 0
    reg = _kv_read_registry()
    for name in to_delete:
        path = os.path.join(KV_DIR, name)
        if os.path.exists(path):
            os.remove(path)
            deleted += 1
        reg.pop(name, None)
    _kv_write_registry(reg)
    return {"deleted": deleted, "kept": keep}


def kv_router_call(action, payload=None):
    """Call the router's /slots/0?action=... with the given payload."""
    return router_post(f"/slots/0?action={action}", payload)


# --- Model roster ---

def parse_presets():
    """Parse presets.ini into a list of {name, model} dicts.
    INI format: [model_name] sections with key=value pairs."""
    presets = []
    if not os.path.exists(PRESETS_FILE):
        return presets
    try:
        with open(PRESETS_FILE, "r") as f:
            current = None
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or line.startswith(";"):
                    continue
                # Section header: [model_name]
                m = re.match(r"^\[(.+?)\]$", line)
                if m:
                    current = m.group(1)
                    if current != "*":  # skip [*] global section
                        presets.append({"name": current, "model": current})
                    continue
                # key = value
                if current and "=" in line:
                    key, val = line.split("=", 1)
                    key = key.strip()
                    val = val.strip()
                    if key == "model":
                        for p in presets:
                            if p["name"] == current:
                                p["model"] = val
                                break
    except Exception:
        pass
    return presets


def scan_models_dir():
    """Scan /opt/models for .gguf files not covered by presets."""
    preset_models = {p["model"].split("/")[-1] for p in parse_presets()}
    others = []
    if os.path.isdir(MODELS_DIR):
        for name in sorted(os.listdir(MODELS_DIR)):
            if not name.endswith(".gguf"):
                continue
            if name in preset_models:
                continue
            others.append(name)
    return others


# --- GPU detection ---

def detect_gpu():
    """Detect GPU vendor and read metrics via amdgpu_top / nvidia-smi / sysfs."""
    import subprocess, json as _json
    vendor = "unknown"
    name = "unknown"
    # Detect vendor from lspci
    try:
        out = subprocess.run(["lspci"], capture_output=True, text=True, timeout=3)
        for line in out.stdout.splitlines():
            if "VGA" in line or "3D" in line or "Display" in line:
                low = line.lower()
                if "nvidia" in low:
                    vendor = "nvidia"
                elif "amd" in low or "ati" in low:
                    vendor = "amd"
                elif "intel" in low:
                    vendor = "intel"
                name = line.split(":", 1)[1].strip() if ":" in line else line
                break
    except Exception:
        pass

    metrics = {}
    if vendor == "amd":
        # Use amdgpu_top for rich metrics (VRAM, power, compute, temp, fan)
        try:
            out = subprocess.run(["/usr/bin/amdgpu_top", "-d", "-J"], capture_output=True, text=True, timeout=10)
            data = _json.loads(out.stdout)
            dgpu = None
            for d in data:
                if d.get("GPU Type") == "dGPU":
                    dgpu = d
                    break
            if dgpu:
                sensors = dgpu.get("Sensors", {})
                vram = dgpu.get("VRAM", {})
                activity = dgpu.get("gpu_activity", {})
                def sv(key):
                    v = sensors.get(key)
                    return v.get("value", 0) if isinstance(v, dict) else 0
                def vv(key):
                    v = vram.get(key)
                    return v.get("value", 0) if isinstance(v, dict) else 0
                def av(key):
                    v = activity.get(key)
                    return v.get("value", 0) if isinstance(v, dict) else 0
                metrics = {
                    "power_w": sv("Average Power"),
                    "temp_c": sv("Edge Temperature"),
                    "freq_mhz": sv("GFX_SCLK"),
                    "vram_used_mib": vv("Total VRAM Usage"),
                    "vram_total_mib": vv("Total VRAM"),
                    "gfx_percent": av("GFX"),
                    "fan_rpm": sv("Fan"),
                }
        except Exception:
            pass
        # Fallback to sysfs
        if not metrics:
            try:
                import glob
                for card in ["card0", "card1"]:
                    for hwmon in glob.glob(f"/sys/class/drm/{card}/device/hwmon/hwmon*"):
                        def rd(n):
                            try:
                                with open(f"{hwmon}/{n}") as f:
                                    return int(f.read().strip())
                            except Exception:
                                return None
                        p, t, fr = rd("power1_input"), rd("temp1_input"), rd("freq1_input")
                        if p:
                            metrics = {"power_w": p / 1e6, "temp_c": (t or 0) / 1000, "freq_mhz": (fr or 0) / 1e6}
                            break
                    if metrics:
                        break
            except Exception:
                pass
    elif vendor == "nvidia":
        try:
            out = subprocess.run(
                ["nvidia-smi", "--query-gpu=power.draw,temperature.gpu,clocks.current.graphics,utilization.gpu,memory.used,memory.total",
                 "--format=csv,noheader,nounits"],
                capture_output=True, text=True, timeout=3)
            parts = out.stdout.strip().split(",")
            if len(parts) >= 6:
                metrics = {
                    "power_w": float(parts[0]),
                    "temp_c": float(parts[1]),
                    "freq_mhz": float(parts[2]),
                    "gfx_percent": float(parts[3]),
                    "vram_used_mib": float(parts[4]),
                    "vram_total_mib": float(parts[5]),
                }
        except Exception:
            pass

    return {"vendor": vendor, "name": name, **metrics}


# --- HTTP handler ---

class SidecarHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass  # silence

    def _json(self, code, obj):
        body = json.dumps(obj).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _html(self, path):
        try:
            with open(path, "rb") as f:
                body = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-cache, no-store, must-revalidate")
            self.end_headers()
            self.wfile.write(body)
        except FileNotFoundError:
            self._json(404, {"error": f"{path} not found"})

    def do_GET(self):
        if self.path == "/":
            self._html(os.path.join(os.path.dirname(os.path.abspath(__file__)), "sidecar.html"))
        elif self.path == "/alt":
            self._html(os.path.join(os.path.dirname(os.path.abspath(__file__)), "alt.html"))
        elif self.path == "/api/kv/snapshots":
            model = loaded_model()
            snaps = kv_list_snapshots(model)
            self._json(200, {"snapshots": snaps, "model": model, "dir": KV_DIR})
        elif self.path == "/api/models/roster":
            presets = parse_presets()
            others = scan_models_dir()
            self._json(200, {"presets": presets, "others": others})
        elif self.path == "/api/gpu":
            self._json(200, detect_gpu())
        elif self.path == "/health":
            self._json(200, {"status": "ok"})
        elif self.path.startswith("/api/prometheus/query_range"):
            # Proxy Prometheus range queries
            query = self.path[len("/api/prometheus/query_range"):]
            if query.startswith("?"):
                query = query[1:]
            try:
                req = urllib.request.Request(
                    f"http://127.0.0.1:9090/api/v1/query_range?{query}",
                    headers={"User-Agent": "curl"},
                )
                resp = urllib.request.urlopen(req, timeout=10)
                self._json(200, json.loads(resp.read().decode("utf-8", errors="replace")))
            except Exception as e:
                self._json(502, {"error": str(e)})
        elif self.path.startswith("/api/prometheus/query"):
            # Proxy Prometheus instant queries
            query = self.path[len("/api/prometheus/query"):]
            if query.startswith("?"):
                query = query[1:]
            try:
                req = urllib.request.Request(
                    f"http://127.0.0.1:9090/api/v1/query?{query}",
                    headers={"User-Agent": "curl"},
                )
                resp = urllib.request.urlopen(req, timeout=10)
                self._json(200, json.loads(resp.read().decode("utf-8", errors="replace")))
            except Exception as e:
                self._json(502, {"error": str(e)})
        else:
            self._json(404, {"error": "not found"})

    def do_POST(self):
        try:
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length) if length else b"{}"
            payload = json.loads(body)
        except Exception:
            payload = {}

        model = loaded_model()

        if self.path == "/api/kv/save":
            filename = payload.get("filename", time.strftime("%Y%m%d-%H%M%S"))
            result = kv_router_call("save", {"model": model, "filename": filename})
            if "error" not in result:
                _kv_tag(filename, model)
            self._json(200, result)

        elif self.path == "/api/kv/restore":
            filename = payload.get("filename")
            if not filename:
                snaps = kv_list_snapshots(model)
                if not snaps:
                    self._json(404, {"error": "no snapshots for loaded model"})
                    return
                filename = snaps[0]["filename"]
            result = kv_router_call("restore", {"model": model, "filename": filename})
            self._json(200, result)

        elif self.path == "/api/kv/erase":
            result = kv_router_call("erase", {"model": model})
            self._json(200, result)

        elif self.path == "/api/kv/swap":
            filename = payload.get("filename", time.strftime("%Y%m%d-%H%M%S"))
            saved = kv_router_call("save", {"model": model, "filename": filename})
            if "error" in saved:
                self._json(200, {"error": saved["error"]})
                return
            _kv_tag(filename, model)
            erased = kv_router_call("erase", {"model": model})
            self._json(200, {"saved": saved, "erased": erased})

        elif self.path == "/api/kv/prune":
            keep = int(payload.get("keep", KV_DEFAULT_KEEP))
            result = kv_prune(keep, model)
            self._json(200, result)

        elif self.path == "/api/models/load":
            model_id = payload.get("model")
            if not model_id:
                self._json(400, {"error": "model required"})
                return
            result = router_post("/v1/models/load", {"model": model_id})
            self._json(200, result)

        elif self.path == "/api/models/unload":
            model_id = payload.get("model")
            if not model_id:
                self._json(400, {"error": "model required"})
                return
            result = router_post("/v1/models/unload", {"model": model_id})
            self._json(200, result)

        else:
            self._json(404, {"error": "not found"})


def main():
    port = int(os.environ.get("SIDECAR_PORT", "8092"))
    server = http.server.HTTPServer(("127.0.0.1", port), SidecarHandler)
    print(f"sidecar listening on 127.0.0.1:{port}")
    print(f"router: {ROUTER_HOST}:{ROUTER_PORT}")
    print(f"kv dir: {KV_DIR}")
    server.serve_forever()


if __name__ == "__main__":
    main()
