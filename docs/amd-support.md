# AMD (amdgpu) GPU Support

This change makes the dashboard work on AMD systems. Previously the GPU path was
NVIDIA-only (`nvidia-smi`); now the backend falls back to the `amdgpu` sysfs
interface, and the frontend gains an AMD hero view + theme behind a nav toggle.
Classic view remains the default and is untouched.

Verified on Python 3.14 against an RX 7900 GRE box: live util/VRAM/power/
temperature/fan/clock/PCIe from sysfs, tenant attributed to the card holding
the model.

## Files

| file | change |
|---|---|
| `fleet-metrics.py` | AMD sysfs backend, backend dispatcher, tenant attribution, query-string-safe routing |
| `index.html` | `--ok` status variables, AMD theme, hero view, view toggle |
| `README.md` | AMD in the feature description + tenant-attribution note |
| `INSTALL.md` | AMD requirements, troubleshooting, verification note |

## Backend — `fleet-metrics.py`

### `parse_gpus()` — dispatcher
- Tries `nvidia-smi` first; if it has no cards, falls back to `parse_amd_gpus()`.
- Sets `_GPU_BACKEND` to `"nvidia"` / `"amd"` / `"none"`.
- One vendor per box — a mixed box would need a merge, and the fleet does not
  run both.

### `parse_amd_gpus()` — the sysfs path
No headless AMD equivalent of `nvidia-smi` exists: `amdgpu_top`/`radeon_top`
are TUIs (panic without a TTY) and `rocm-smi` is a full ROCm install. Everything
they show is already in sysfs, which is what this reads:

| field | sysfs source | units handled |
|---|---|---|
| `gpu_util` | `device/gpu_busy_percent` | % |
| `mem_used/total_mib` | `device/mem_info_vram_used/total` | bytes → MiB |
| `temp_c` | `hwmon*/temp1_input` | 1/100 °C |
| `power_w` | `hwmon*/power1_average` | µW |
| `power_limit_w` | `hwmon*/power1_cap` | µW |
| `fan_pct` | `hwmon*/fan1_input` + `fan1_min/max` | RPM → % of range |
| `sm_clock_mhz` | `hwmon*/freq1_input` | Hz |
| `pcie_gen` / `pcie_width` | `device/current_link_speed/width` | GT/s → gen (`_PCIE_GTS_TO_GEN`) |

- Cards are enumerated under `/sys/class/drm/card*/device`, filtered by
  `DRIVER=amdgpu` in `uevent`.
- hwmon may be nested (`device/hwmon/hwmonN`, kernels ≥ 5.x) or flat
  (`device/hwmonN`) — both are probed.
- PCIe reports the **negotiated** link, not slot/card maximums, so `*_max` stay
  0 and the frontend renders a bare "LINK" (no "x of x16" framing).
- **Degradation, not failure:** `_sysfs_int()` returns `None` on any error, so
  one unreadable attribute costs that field (0 / `—`), never the card or the
  panel. A card is only dropped if `gpu_busy_percent` itself is missing.

### `compute_card` flag
- NVIDIA: always `true` — `nvidia-smi` only enumerates compute cards.
- AMD: `true` when the card carries a `power1_cap` sensor. Display iGPUs have
  none, and their busy%/VRAM are driven by the display engine — the hero view
  strips them instead of hero-ing.

### `attach_gpu_tenants()` — tenant attribution
- **NVIDIA:** ground truth from `nvidia-smi --query-compute-apps`
  (`attach_gpu_procs`), unchanged.
- **AMD:** the driver exposes no per-process VRAM (no
  `--query-compute-apps` equivalent), so the worker is **attributed** to the
  card holding the most VRAM, flagged `tenant_estimated`.
  - A 128 MiB threshold keeps an idle desktop from reading as "serving" —
    an iGPU's display engine alone holds over a GiB and spins busy% to 100.
  - "Most VRAM" picks the compute card when iGPU and dGPU coexist.
  - The frontend renders estimated tenants with a `~` prefix — "likely", not
    fact.

### Routing fix
The HTTP handler now matches on the path **without** the query string
(`self.path.split('?', 1)[0]`), so `?view=amd` deep-links don't 404.

## Frontend — `index.html`

### Status green isolated from chrome
New `--ok` / `--ok-dim` / `--ok-glow` variables carry "online/serving" green,
separated from `--accent`. The AMD theme turns chrome accent **red**, and
status must stay green or it reads as an error. Classic theme: `--ok` aliases
`--accent`, so nothing changes there.

### AMD theme (`body.theme-amd`)
AMD-red (`#ED1C24`) chrome on red-tinted charcoal; status/amber/rose/red
colors untouched — they are meaning, not chrome.

### AMD hero view (`body.view-amd`)
`#hero` renders **one panel per compute card**: left = card telemetry (same
fields as the classic GPU card, same null/0 → `—` guards), right = the worker
it serves (from `llama_8001`): decode/prompt tok/s, ctx fill, request queue,
TTFT/ITL. Display cards (`compute_card === false`) collapse to a one-line
strip. The worker pane sits on the card carrying the backend's tenant — never
index 0. Absorbs the five top metric cards and the GPU grid; everything else
renders exactly as classic.

### View toggle
`CLASSIC | AMD` buttons in the nav. Precedence: **`?view=` param >
localStorage (`llm-serve-view`) > classic**. Storage and history calls are
guarded — if either is blocked (strict privacy modes), the view still applies
for the load; it just won't persist. A blocked read must not kill the page
before `poll()` starts.

## Docs

- **README** — backend described as `nvidia-smi` (NVIDIA) or amdgpu sysfs
  (AMD); tenant-attribution note; AMD hero view bullet under Notes.
- **INSTALL** — requirements broadened to "GPU access" (NVIDIA or AMD);
  AMD troubleshooting (`gpu_busy_percent` existence check); verification note
  extended with the RX 7900 GRE / Python 3.14 box.
