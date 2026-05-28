# API Benchmark Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the Python OpenAI-compatible latency benchmark into CC Switch as native Rust/Tauri functionality.

**Architecture:** Add a focused Rust service that can list benchmarkable provider/model entries and run a streaming `/v1/chat/completions` benchmark against selected entries. Expose it through Tauri commands and a small TypeScript API wrapper so the React UI can use it later.

**Tech Stack:** Rust 1.85, reqwest streaming, serde, Tauri commands, TypeScript invoke wrappers.

---

### Task 1: Benchmark Core Types And Helpers

**Files:**
- Create: `src-tauri/src/services/api_benchmark.rs`
- Modify: `src-tauri/src/services/mod.rs`

- [x] **Step 1: Write failing Rust tests**

Add tests for prompt lookup, URL normalization, usage parsing, and throughput metric calculation.

- [x] **Step 2: Verify RED**

Run: `cargo test api_benchmark --lib`
Expected: fails because `api_benchmark` module does not exist.

- [x] **Step 3: Implement minimal core module**

Define benchmark prompt enum, result row, summary helpers, OpenAI stream usage parsing, and URL normalization.

- [x] **Step 4: Verify GREEN**

Run: `cargo test api_benchmark --lib`
Expected: benchmark helper tests pass.

### Task 2: Provider Entry Resolution

**Files:**
- Modify: `src-tauri/src/services/api_benchmark.rs`

- [x] **Step 1: Write failing tests**

Add tests that convert Codex TOML settings and OpenClaw JSON settings into benchmark entries while omitting missing API keys.

- [x] **Step 2: Verify RED**

Run: `cargo test api_benchmark --lib`
Expected: fails because provider extraction is missing.

- [x] **Step 3: Implement extraction**

Read existing `Provider` settings for Codex and OpenClaw/OpenCode style providers and produce safe list entries with redacted key metadata.

- [x] **Step 4: Verify GREEN**

Run: `cargo test api_benchmark --lib`
Expected: extraction tests pass.

### Task 3: Tauri Commands And TS API

**Files:**
- Create: `src-tauri/src/commands/api_benchmark.rs`
- Modify: `src-tauri/src/commands/mod.rs`
- Modify: `src-tauri/src/lib.rs`
- Create: `src/lib/api/api-benchmark.ts`

- [x] **Step 1: Write failing compile/API surface**

Add command declarations and TypeScript wrapper references to new invoke names.

- [x] **Step 2: Implement commands**

Expose `list_api_benchmark_entries` and `run_api_benchmark`.

- [x] **Step 3: Verify**

Run: `cargo test api_benchmark --lib`
Run: `pnpm typecheck`
Expected: both pass or reveal unrelated baseline issues.

### Task 4: Final Verification

**Files:**
- All changed files

- [x] **Step 1: Run focused Rust tests**

Run: `cargo test api_benchmark --lib`

- [x] **Step 2: Run TypeScript typecheck**

Run: `pnpm typecheck`

- [x] **Step 3: Report usage**

Explain that the backend now supports listing ccswitch provider configs and running selected indices; UI wiring can be the next step.
