# macOS Client Support — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add macOS client support to DevBridge so a Mac on WireGuard can receive print jobs and print locally, identical to Windows clients.

**Architecture:** Fill in existing `#[cfg(not(target_os = "windows"))]` stubs with CUPS/launchd/Unix socket implementations. Add `backend_cups.rs` module, macOS installer scripts, and CI macOS build job. No new abstractions.

**Tech Stack:** Rust, CUPS CLI (`lp`/`lpstat`), launchd plists, Unix domain sockets, Tauri 2 (app/dmg bundles), GitHub Actions `macos-latest` runner.

**Spec:** `docs/superpowers/specs/2026-04-04-macos-client-design.md`

---

See plan file for full task breakdown.
