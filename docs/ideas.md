# Ideas

Future ideas not yet promoted to the beads backlog. When one becomes actionable, file a bead and remove it from here.

---

## Fork WSLg to fix Windows snap/tiling

WSLg windows can't use Windows snap layouts, drag-to-edge tiling, or proper fullscreen. The canonical issue is [microsoft/wslg#22](https://github.com/microsoft/wslg/issues/22) — open since April 2021, no roadmap commitment from Microsoft.

**Root cause**: WSLg's FreeRDP RAIL layer doesn't expose the right DWM window management hints (`WM_SIZING`, `WM_NCCALCSIZE`) to Windows. The fix is contained to the RAIL channel bridge — C code, not the full Weston compositor.

**Approach**: Fork [microsoft/wslg](https://github.com/microsoft/wslg) (MIT license), fix the RAIL window hints, upstream a PR. Could throw agent swarm at the C codebase to understand the RAIL protocol layer quickly.

**Why it matters**: Would let therminal run via WSLg with full Windows window management as a fallback to native builds. Also benefits every other Linux GUI app on WSL2.
