# Dashboard Redesign + KV Cache Control — Design (Revised)

Date: 2026-09-14
Status: PROPOSED (pending user approval)
Architecture: **Grafana + Prometheus** (metrics) + **minimal Python sidecar** (controls + bespoke UI)

## 1. Purpose

Replace the monolithic `fleet-metrics.py` + `index.html` dashboard with a split architecture:
- **Grafana + Prometheus** (Docker) for metrics visualization — sparklines, mobile view,
  rearrangeable panels, decode/prompt/ctx, GPU, power, network. No custom rendering code.
- **Minimal Python sidecar** for what Grafana can't do: KV cache save/restore/swap/erase +
  prune, model LOAD/UNLOAD, model roster, and a bespoke widget-based control page.

## 2. Scope

### In scope
- Docker Compose stack: Prometheus + Grafana.
- Grafana dashboard JSON with panels from existing `llamacpp:*` / `vllm:*` Prometheus series.
- New minimal Python sidecar server (replaces `fleet-metrics.py`):
  - `/api/kv/{snapshots,save,restore,erase,swap,prune}` proxying the router's `/slots/0`,
    with a model-tagged snapshot registry (`kv-snapshots.json`).
  - `/api/models/{load,unload}` proxying the router's model actions.
  - Serves the sidecar HTML page.
- Bespoke sidecar UI (the original intended design): dark terminal aesthetic,
  collapsible/reorderable widgets (localStorage), KV panel, model roster, key live stats.
- Second bespoke comparison dashboard (`/alt`).

### Non-goals
- No custom metrics rendering (Grafana handles it).
- No new inference engine.
- No auth (loopback-only).
- The second design is a comparison artifact, not a production replacement.

## 3. Architecture / data flow

### 3.1 Prometheus + Grafana (Docker)
- **Prometheus** scrapes:
  - llama router `localhost:8080/metrics` (needs `?model=` for per-model gauges — handled
    by the router's own export).
  - Worker ports (ephemeral, resolved from `/v1/models`).
- **Grafana** reads Prometheus as its data source; dashboard panels use the existing series:
  - `llamacpp:predicted_tokens_seconds` → decode tok/s
  - `llamacpp:prompt_tokens_seconds` → prompt tok/s
  - `llamacpp:n_tokens_max` / `n_ctx` (from `/props`) → ctx fill %
  - GPU: `nvidia-smi` (via a node exporter or a small exporter) — or manual panels.
  - System: node exporter for RAM/CPU.
  - Network: node exporter.
- Mobile: Grafana is responsive out of the box.

### 3.2 Sidecar (new minimal Python server)
- Loopback-only, stdlib-only, no deps.
- **KV controller**: same as the `fleet-metrics.py` KV work already done —
  `_loaded_model()`, `_kv_list_snapshots(model)`, `_kv_prune(keep, model)`,
  `_kv_router_call(action, payload)`, `_kv_tag(filename, model)`, and the
  `/api/kv/*` + `/api/models/*` handlers.
- **Model roster**: `parse_presets()` + `scan_models_dir()` (already done).
- Serves the sidecar HTML at `/` and the second design at `/alt`.
- **Retires `fleet-metrics.py`** — the GPU/system/network scraping is gone (Grafana handles it).

### 3.3 Sidecar UI (bespoke, original intended design)
- Dark terminal aesthetic (CSS custom properties, preserved from the original).
- **Widget system**: each widget a card with stable `id`; grid rendered from an ordered
  list of widget ids; user can collapse (hide body, keep header) and reorder (drag or
  up/down); order + collapsed set persisted in `localStorage` (validated, falls back to
  default); "reset layout" control.
- **Widgets**:
  - Model hero (loaded model, ctx fill, decode/prompt — quick glance, links to Grafana)
  - KV cache panel (list/save/restore/swap/erase/prune)
  - Model roster (presets + collapsible "Other models")
  - Key stats (seats, queue, latency — the few numbers you want without opening Grafana)
- **KV panel**: list snapshots (name, model tag, size MB, age), Save/Restore/Swap/Erase/Prune
  with confirm dialogs for destructive ops; show router outcome or error; disable actions
  when no model loaded; escaped DOM.
- Responsive: desktop 3-col, tablet 2-col, mobile 1-col stack.

## 4. KV cache panel

### 4.1 Features
- **List**: snapshots (name, model tag, size MB, age), most recent first, filtered by
  loaded model (KV caches are model-specific).
- **Save**: save current slot to a snapshot (default name = timestamp), tagged with model.
- **Restore**: load a snapshot back into the slot (default = most recent for the loaded model).
- **Swap**: save + erase in one action (rollback hint shown).
- **Erase**: clear the slot (confirm dialog — active context is lost).
- **Prune**: keep the N most recent snapshots **for the loaded model**, delete the rest.
  N configurable (default 3). Model-scoped — other models' snapshots untouched.

### 4.2 Model tagging
- Registry: `kv-snapshots.json` in the snapshot dir — `filename -> {model, size, mtime}`.
- Save/swap tag the snapshot with the loaded model.
- List/restore/prune filter by the loaded model.
- The router strips the `.bin` extension on save (a file named `foo.bin` lands as `foo`),
  so the registry key is the bare name.

### 4.3 Safety
- Every KV action returns the router's JSON; the UI shows the outcome
  (`n_saved`, `n_erased`, `n_restored`) or the error.
- Erase and prune are destructive → confirm dialog in the UI.
- All snapshot filenames are escaped before DOM insert.

## 5. Interfaces

### 5.1 Sidecar endpoints
| Method | Path | Body | Returns |
|--------|------|------|---------|
| GET | `/api/kv/snapshots` | `?model=<id>` (optional) | `{snapshots:[{filename,model,size,mtime}], model, dir}` |
| POST | `/api/kv/save` | `{filename?}` | router save JSON |
| POST | `/api/kv/restore` | `{filename?}` | router restore JSON (model-scoped) |
| POST | `/api/kv/erase` | — | router erase JSON |
| POST | `/api/kv/swap` | `{filename?}` | `{saved, erased}` |
| POST | `/api/kv/prune` | `{keep?}` | `{deleted, kept}` (model-scoped) |
| POST | `/api/models/load` | `{model}` | router load JSON |
| POST | `/api/models/unload` | `{model}` | router unload JSON |
| GET | `/` | — | sidecar HTML |
| GET | `/alt` | — | second design HTML |

### 5.2 Prometheus scrape targets
- `localhost:8080/metrics` (router)
- Worker ports (from `/v1/models`) — or a static config if ports are stable.
- Node exporter for system/GPU/network (if added).

## 6. Error handling
- Router down → KV panel shows "router unreachable" and disables actions.
- KV op fails → show the router's error message; never a silent success.
- Snapshot dir missing/unreadable → `/api/kv/snapshots` returns `{snapshots:[]}` and prune
  returns `{deleted:0, kept:0}`; UI shows "no snapshots".
- Grafana/Prometheus down → sidecar still works (KV + model controls); metrics just
  unavailable in Grafana.

## 7. Testing
- **Prometheus/Grafana**: containers up, scraping, dashboard renders.
- **Sidecar KV**: save → list → restore → erase → prune against live router on 8080;
  model tagging correct; model-scoped filter works.
- **Sidecar UI**: `node --check` on extracted JS; widget persistence across reload;
  mobile viewport check.
- **Second design**: consumes same sidecar API; renders with no console errors.

## 8. Failure modes

| # | Failure | Severity | Mitigation |
|---|---------|----------|------------|
| F1 | Router on a different port than assumed | Critical | `LLAMA_ROUTER` env + auto-detect loaded model from `/v1/models`; default 8080. |
| F2 | KV op sent to a model that isn't loaded | Critical | Auto-detect loaded model; if none loaded, disable save/restore/swap and show "no model loaded". |
| F3 | Prune deletes a snapshot in use | Minor | Prune is model-scoped, by age; restore reads before prune; confirm dialog gates prune. |
| F4 | Grafana/Prometheus not scraping worker ports | Critical | Worker ports are ephemeral; use a scrape config that resolves them, or a static list. Document the limitation. |
| F5 | Mobile layout breaks | Minor | Responsive grid + single-column stack; tested at 375px. |
| F6 | Widget reorder/collapse state corrupts on load | Minor | `localStorage` state validated (known widget ids only); invalid falls back to default. "Reset layout" always available. |
| F7 | Second design drifts from sidecar API | Critical | Consumes the exact same endpoints; no new endpoints. If a widget needs data the sidecar doesn't expose, it is cut. |

## 9. Conventions
- Loopback-only, no auth.
- Sidecar: stdlib-only Python, no new deps.
- Docker Compose for Prometheus + Grafana.
- Frontend: escaped DOM, 2s poll, delegated event handlers.
- Env overrides: `LLAMA_ROUTER`, `KV_SNAPSHOT_DIR`, `KV_DEFAULT_KEEP`, `MODELS_DIR`.
