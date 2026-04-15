---
name: PocketForge (Termux fork)
description: pocketforge is the user's Termux fork — merges codefactory's xterm.js/Axum PWA pages with native Termux terminals; portable-pty patched for Android PTY support
type: reference
---

**pocketforge** is the user's Termux fork that combines:
- Native Termux terminal sessions
- codefactory's WebView pages (xterm.js + Axum Rust backend)

**Why:** codefactory (~/projects/codefactory) was a standalone PWA terminal complex; pocketforge merges it into the Termux app itself so native terminals + web-based pages coexist. Stock portable-pty works fine on Termux — the termios patch in codefactory just reduced input lag slightly on native terminals, wasn't required.

**How to apply:** When the user mentions pocketforge, Termux, or mobile terminal work, this is the project.
