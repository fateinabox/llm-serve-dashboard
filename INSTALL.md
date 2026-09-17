# Install

Rust sidecar for the LLM serve dashboard. Single static binary, no runtime, no deps to install.

---

## 1. What you need

| | |
|---|---|
| **Rust toolchain** | `cargo --version`. Build once, run the binary forever. |
| **GPU access** | NVIDIA: `nvidia-smi` on PATH. AMD: `amdgpu` kernel driver + `amdgpu_top` (optional, sysfs fallback). |
| **Tailscale** (optional) | For remote access over the tailnet with automatic HTTPS. |
| **A browser** | Any current one. |

## 2. Build

```bash
cargo build --release
```

Produces `target/release/sidecar` (~6 MB static binary).

## 3. Run

```bash
./target/release/sidecar
```

Or with custom config:

```bash
LLAMA_ROUTER=8080 SIDECAR_PORT=8092 KV_SNAPSHOT_DIR=/opt/models/kvcache ./target/release/sidecar
```

Open <http://127.0.0.1:8092/> in a browser.

### Tailscale (remote access)

```bash
# Expose the sidecar over HTTPS on the tailnet (port 8443)
tailscale serve --bg --https 8443 8092
```

Then open `https://<your-node>.<tailnet>.ts.net:8443/` from any device on your tailnet.
Cert is auto-issued by Tailscale. No app-side TLS or auth needed.

### Systemd user service

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/llm-sidecar.service <<EOF
[Unit]
Description=LLM Serve Sidecar
After=network.target

[Service]
Type=simple
WorkingDirectory=/opt/llm-serve-dashboard
ExecStart=/opt/llm-serve-dashboard/target/release/sidecar
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now llm-sidecar
```

## 4. Environment variables

| Var | Default | Purpose |
|-----|---------|---------|
| `LLAMA_ROUTER` | `8080` | Router port |
| `SIDECAR_PORT` | `8092` | Sidecar listen port |
| `KV_SNAPSHOT_DIR` | `/opt/models/kvcache` | KV snapshot storage |
| `KV_DEFAULT_KEEP` | `3` | Default prune keep count |
| `MODELS_DIR` | `/opt/models` | Model directory for roster scan |

## 5. Verify

```bash
curl -s http://127.0.0.1:8092/health
curl -s http://127.0.0.1:8092/api/gpu
curl -s http://127.0.0.1:8092/api/metrics
```

## 6. Routes

| Route | Method | Description |
|-------|--------|-------------|
| `/` | GET | Dashboard HTML |
| `/alt` | GET | Alternate design HTML |
| `/health` | GET | Liveness check |
| `/api/gpu` | GET | GPU vendor + metrics |
| `/api/metrics` | GET | Worker metrics (TPS, ctx, spec) |
| `/api/system` | GET | RAM, CPU, disk, temps |
| `/api/slots` | GET | Router slot data |
| `/api/log` | GET | Journalctl tail (200 lines) |
| `/api/models/roster` | GET | Presets + scanned models |
| `/api/kv/snapshots` | GET | KV snapshot list |
| `/api/kv/save` | POST | Save KV snapshot |
| `/api/kv/restore` | POST | Restore KV snapshot |
| `/api/kv/erase` | POST | Erase KV cache |
| `/api/kv/swap` | POST | Save + erase |
| `/api/kv/prune` | POST | Prune old snapshots |
| `/api/models/load` | POST | Load model via router |
| `/api/models/unload` | POST | Unload model via router |
