# Fix Widget System Race Condition, Rate Computation, Context Grid, Log Drawer

## Scope
Fix four findings in `sidecar.html` WidgetSystem:

1. **F1 — Init/Poll Race**: Widgets render before async data arrives; no re-render trigger after responses populate state. User chose: add loading skeleton/spinner (least invasive).

2. **F2 — Rate Mislabeling**: `decode_tokens_seconds` / `prompt_tokens_seconds` are cumulative seconds, not tok/s rates. User chose: compute actual tok/s from deltas with stateful tracking.

3. **F3 — Context Grid Overflow**: Generates N cells from cache count but SVG skeleton has only 50 rects; silently drops data for large contexts. User chose: scale to fixed 50-cell heatmap using proportional fill.

4. **F4 — Log Drawer Inverted**: Saved state "true" (was open) triggers adding `.collapsed`, so it always starts collapsed. One-line boolean inversion fix.

## Files
- `sidecar.html` — all changes in the `<script>` block (~lines 800-1633)

## Approach
- F1: Show loading skeleton on first render, swap to real data when poll succeeds
- F2: Track previous slot snapshots as deltas; compute tok/s = Δtokens / Δtime
- F3: Map ctx_fill_pct directly to cell count out of 50 (proportional heatmap)
- F4: Fix the single boolean inversion in DOMContentLoaded handler
