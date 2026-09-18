# Model Presets Configuration Dialog — Design

**Date:** 2026-07-08
**Status:** Draft — pending approval

## Scope

Add a standalone modal dialog for viewing, editing, creating, and deleting model presets in `presets.ini`. The sidecar re-reads the file immediately on save so changes take effect without a service restart.

### In scope
- New API endpoints: `GET /api/presets`, `PUT /api/presets`
- New modal dialog in `sidecar.html` (hybrid form/raw INI editor)
- CRUD: view, edit key-value pairs, add preset, delete preset
- Live reload: sidecar re-parses `presets.ini` after write; UI updates on next poll
- Trigger: button in the header bar (next to settings gear)

### Non-goals
- No per-key validation (INI values are passed through to llama-server; invalid values surface as load errors)
- No file history/undo
- No multi-file support (single `presets.ini` only)
- No permission model (local-only tool, single user)

## Architecture

### Data flow

```
Browser
  │  GET /api/presets
  ▼
Sidecar ──► reads presets.ini ──► parses sections ──► JSON array
  │
  │  PUT /api/presets  (body: full INI text)
  ▼
Sidecar ──► writes presets.ini ──► re-parses ──► 200 OK
  │
  ▼
Next poll: GET /api/metrics ──► parse_presets() ──► fresh presets in UI
```

### Rust changes (`src/main.rs`)

**New functions:**

```rust
fn read_presets_raw(cfg: &Config) -> Option<String>
    // Returns file contents or None if missing.

fn write_presets_raw(cfg: &Config, content: &str) -> Result<(), String>
    // Validates content is non-empty, writes atomically (tmp + rename).

async fn get_presets(State(state): State<AppState>) -> Json<Value>
    // Returns { presets: [...], raw: "..." } — parsed sections + raw INI text.

async fn put_presets(State(state): State<AppState>, body: axum::extract::RawBody) -> Response
    // Body: raw INI text (Content-Type: text/plain or application/json with {"raw": "..."}).
    // Writes file, re-parses, returns 200 with updated presets.
```

**Existing `parse_presets`** stays as-is (returns `Vec<Value>` with `name` + `model`). The new `get_presets` endpoint returns both the parsed list and the raw text so the dialog can show either view.

**New routes:**
```rust
.route("/api/presets", get(get_presets))
.route("/api/presets", put(put_presets))
```

### Frontend changes (`sidecar.html`)

**New HTML:**
- `<button id="presets-btn">⚙ Presets</button>` in the header bar
- `<div class="modal-backdrop" id="presets-backdrop">`
- `<div class="modal-panel" id="presets-panel">` with:
  - Header: "Model Presets" + close button
  - Preset selector: `<select>` listing all preset names
  - Toggle: "Form view" / "Raw INI" buttons
  - **Form view:** for selected preset, list of `key=value` rows with:
    - Editable `<input>` for each value
    - "Add key" row (key input + value input + add button)
    - "Remove" button per row
    - "Add preset" / "Delete preset" buttons
  - **Raw view:** `<textarea>` with full INI content, monospace
  - Save button (enabled in both views)
  - Status line (success/error)

**New JS functions:**
```js
async function openPresetsDialog()     // fetch /api/presets, populate dialog
async function closePresetsDialog()    // hide dialog
async function savePresets()           // PUT /api/presets with raw INI text
function switchPresetView(mode)        // form ↔ raw toggle
function renderPresetForm(preset)      // build key-value rows for selected preset
function onPresetKeyChange(...)        // update in-memory preset object
function addPresetKey(...)             // add new key=value row
function removePresetKey(...)          // remove key=value row
function addNewPreset(name)            // create new [name] section
function deletePreset(name)            // remove [name] section
```

**INI serialization:** The form view maintains an in-memory object `{ "*": {key:val,...}, "PresetName": {key:val,...} }`. On save, serialize to INI text (sections in order, `# comment` lines preserved from raw view if in raw mode). The raw view edits the text directly.

**Event wiring:** After successful save, call `WidgetSystem.emit(EVT.MODEL_CHANGE, { model: currentData.model })` to trigger roster re-render with fresh presets.

## Interfaces

### `GET /api/presets`
```
Response 200:
{
  "presets": [{"name": "Qwen3.8-27B-GSQ-RCO-IQ3_XXS-mtp", "model": "/opt/models/...gguf"}, ...],
  "raw": "[*]\nthreads = 8\n..."
}
```

### `PUT /api/presets`
```
Request: Content-Type: text/plain
Body: full INI text

Response 200:
{
  "ok": true,
  "presets": [...]
}

Response 400:
{ "error": "empty or invalid INI content" }
```

## Error handling

- **File missing:** `GET` returns `{ presets: [], raw: "" }` (200, not 404).
- **Write failure:** `PUT` returns 500 with error message.
- **Empty INI:** `PUT` rejects with 400.
- **Dialog open + poll:** Dialog reads from `currentData.presets` (already polled); no extra fetch needed after save.

## Failure modes

| # | Failure | Severity | Mitigation |
|---|---------|----------|------------|
| F1 | User saves malformed INI; llama-server rejects on next load | Minor | Load error surfaces in UI; user can revert via raw view |
| F2 | Concurrent edit: two browser tabs open dialog, one overwrites the other | Minor | Single-user local tool; no mitigation needed |
| F3 | `presets.ini` permissions: sidecar runs as `llama` user, file owned by root | Critical | Ensure file is writable by `llama` user; document in INSTALL.md |

## Testing

- Unit: `parse_presets` already tested implicitly via `/api/metrics`.
- Manual: open dialog, edit a preset, save, verify `/api/metrics` returns updated preset on next poll.
- Edge: delete all presets, save, verify roster shows empty preset list.
