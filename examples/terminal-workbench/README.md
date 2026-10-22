# Native Shell operations workbench

A dark operations console built with NSH primitives, virtualized lists,
recorded canvas charts and unstyled base dialogs. The app includes a workspace
sidebar, service health metrics, a searchable fleet, a service inspector,
an activity timeline and an interactive deployment flow. The 72 services
are deterministic local data; all changes stay in the current session.

```sh
nsh run examples/terminal-workbench --target tui
```

The compact layout fits a **120×34** terminal with seven complete service
rows, metrics and a small traffic chart. At **120×30** and **100×30**, it
keeps eight service rows visible by folding away the chart and secondary
metrics. The inspector opens at **140 columns** and wider; **160×50** shows
all three panes. The selected service remains visible in the footer.
Resizing the terminal relays out the same view.

The same app runs in a graphical window:

```sh
nsh run examples/terminal-workbench --target linux
nsh run examples/terminal-workbench --target macos
nsh run examples/terminal-workbench --target windows
```

Choose the target for your OS and resize its window to 1280×800 points for
the complete workbench. A 960×640-point window uses the compact layout.

## Try it

- Search for `gateway` to show two matching services, or `王宁` to find
  services owned by the Chinese-speaking team. All, Healthy and Degraded
  combine with the current search.
- Click a service to update the inspector and footer. Scroll the fleet to
  browse all 72 rows; Tab and Shift+Tab move keyboard focus.
- Choose Incidents to show the seven services that need attention, or
  Releases for the eight recent releases. Each section resets search and
  status filters.
- Choose Deploy update, edit the release note and confirm Queue deployment.
  The service becomes Queued, the inspector shows `v2.19.0 queued`, and the
  activity timeline and footer record the deployment. Cancel or Escape
  dismisses the dialog.
- On a wide terminal, open Effects in the inspector to compare blend modes,
  animate a layer, soften the local image and see a bundled Pacifico font
  drawn on Canvas. Options opens a native popup menu for the same controls.
  Guide opens a local HTML service runbook with a reload action.
- Ctrl+Q closes the terminal host.

## Reproducible interaction

From a source checkout, build the bundle with the existing CLI and run the
terminal host headlessly:

```sh
./target/debug/nsh build examples/terminal-workbench -o target/workbench
./target/debug/nsh-tui-host --bundle target/workbench/bundle.nshc --size 160x50 \
  --script "wait 300; key Ctrl+a; type gateway; expect 2 services; expect Selected: api-gateway; tap 40 13; expect Selected: edge-gateway; tap 148 1; expect Deploy service; key Tab; key Tab; key Enter; expect Deployment queued; wait 300; expect v2.19.0; tap 8 9; expect Active incidents; tap 8 11; expect Release history; resize 120 34; wait 200; expect Release history; expect Traffic overview; resize 100 30; wait 200; expect Release history; expect-focus; quit"
```

The checks exercise search, selection, modal confirmation, section
navigation and responsive resizing. Pointer coordinates use terminal
cells and assume the initial 160×50 viewport.
The 300 ms pause after confirmation lets the dialog finish its exit
animation before the next background action.

The optional inspector has a separate checked script:

```sh
./target/debug/nsh-tui-host --bundle target/workbench/bundle.nshc --size 160x50 \
  --script "wait 700; tap 142 5; wait 800; expect Surface effects; expect Canvas lettering; tap 151 6; expect Soft focus; key Enter; wait 500; expect Soft focus / 3pt; tap 153 5; wait 1000; expect Rollout checklist; expect Runbook ready; tap 153 6; wait 500; expect Runbook ready; tap 132 5; expect Activity timeline; quit"
```

The image and font are bundled assets; the runbook uses inline HTML. These
samples work offline. Pacifico's license is included in `assets/OFL.txt`
and in the font's license metadata, so packaged copies retain it.
Text assertions target labels around the raster samples; interactive
rendering shows their colors, blur, blending and lettering.

| Control | Test ID or accessibility label | Initial cell position |
|---|---|---|
| Services section | `nav-services`; `Open Services` | `8 7` |
| Incidents section | `nav-incidents`; `Open Incidents` | `8 9` |
| Releases section | `nav-releases`; `Open Releases` | `8 11` |
| Search | `service-search`; `Search services` | `40 9` |
| Status filters | `filter-all`, `filter-healthy`, `filter-degraded` | Tab forward from search |
| Service row | `service-<id>`; `Inspect <name>` | First row `40 11`; second `40 13` |
| Deploy action | `deploy-update`; `+ Deploy update` | `148 1` (`108 1` at 120 columns) |
| Inspector deploy action | `deploy-selected`; `Deploy this service` | Right inspector |
| Inspector tabs | `inspector-details`, `inspector-effects`, `inspector-guide` | `132 5`, `142 5`, `153 5` |
| Effects popup | `effects-options`; `Options` | `151 6` |
| Blend / pulse | `effects-blend`, `effects-pulse` | `133 14`, `143 14` |
| Image treatment | `effects-blur`; `Soften` / `Sharpen` | `147 21` |
| Runbook reload | `runbook-reload`; `Reload` | `153 6` |
| Release note | `release-note`; `Release note` | First focusable control in the dialog |
| Confirm / cancel | `Confirm deployment`; `Cancel deployment` | Tab twice / once from release note |

The table container is `service-table`, the chart is `traffic-chart`, the
font canvas is `font-sample` and the HTML reader is `service-runbook`.
