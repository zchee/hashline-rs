// Copyright 2026 The hashline-rs Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! One-shot resource, filesystem-cache, and symbolized-profile Phase 0 probes.

#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    fs::File,
    hint::black_box,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use hashline::{
    config::SchemeConfig,
    edit::{HashlineOp, apply::apply_edits, run_edit},
    grep::run_grep,
    index::FileIndex,
    protocol::{
        EditOperation, EditRequest, GrepOutputMode, GrepRequest, PageCursor, Position, ReadRequest,
    },
    read::{format_hashline_content, run_read},
    scheme::Scheme,
    snapshot::Snapshot,
    util::Workspace,
};
use serde_json::{Value, json};

// This binary uses the shared resource subset; the Criterion target uses the
// remaining prototype functions from the same module.
#[allow(dead_code)]
mod phase0_workloads;

const GREP_FILE_COUNT: usize = 2_000;
const GREP_LINES_PER_FILE: usize = 40;
const GREP_RARE_TOKEN: &str = "zqxj7_rare_marker_unique";
const GREP_COMMON_TOKEN: &str = "value";

#[derive(Debug)]
struct CountingAllocator {
    enabled: AtomicBool,
    allocation_calls: AtomicUsize,
    reallocation_calls: AtomicUsize,
    deallocation_calls: AtomicUsize,
    allocated_bytes: AtomicUsize,
    deallocated_bytes: AtomicUsize,
    live_bytes: AtomicUsize,
    peak_live_bytes: AtomicUsize,
}

impl CountingAllocator {
    const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            allocation_calls: AtomicUsize::new(0),
            reallocation_calls: AtomicUsize::new(0),
            deallocation_calls: AtomicUsize::new(0),
            allocated_bytes: AtomicUsize::new(0),
            deallocated_bytes: AtomicUsize::new(0),
            live_bytes: AtomicUsize::new(0),
            peak_live_bytes: AtomicUsize::new(0),
        }
    }

    fn begin(&self) {
        self.allocation_calls.store(0, Ordering::Relaxed);
        self.reallocation_calls.store(0, Ordering::Relaxed);
        self.deallocation_calls.store(0, Ordering::Relaxed);
        self.allocated_bytes.store(0, Ordering::Relaxed);
        self.deallocated_bytes.store(0, Ordering::Relaxed);
        self.live_bytes.store(0, Ordering::Relaxed);
        self.peak_live_bytes.store(0, Ordering::Relaxed);
        self.enabled.store(true, Ordering::SeqCst);
    }

    fn finish(&self) -> AllocationStats {
        self.enabled.store(false, Ordering::SeqCst);
        AllocationStats {
            allocation_calls: self.allocation_calls.load(Ordering::Relaxed),
            reallocation_calls: self.reallocation_calls.load(Ordering::Relaxed),
            deallocation_calls: self.deallocation_calls.load(Ordering::Relaxed),
            allocated_bytes: self.allocated_bytes.load(Ordering::Relaxed),
            deallocated_bytes: self.deallocated_bytes.load(Ordering::Relaxed),
            live_bytes: self.live_bytes.load(Ordering::Relaxed),
            peak_live_bytes: self.peak_live_bytes.load(Ordering::Relaxed),
        }
    }

    fn add_live_bytes(&self, bytes: usize) {
        let live = self.live_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak_live_bytes.fetch_max(live, Ordering::Relaxed);
    }

    fn subtract_live_bytes(&self, bytes: usize) {
        let mut live = self.live_bytes.load(Ordering::Relaxed);
        loop {
            let next = live.saturating_sub(bytes);
            match self.live_bytes.compare_exchange_weak(
                live,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => live = actual,
            }
        }
    }
}

// SAFETY: Every operation delegates to System with the original pointer and
// layout. The atomics observe sizes only and never affect allocation results.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegating the caller-provided layout to System preserves the
        // GlobalAlloc contract.
        let pointer = unsafe { System.alloc(layout) };
        if self.enabled.load(Ordering::Relaxed) && !pointer.is_null() {
            self.allocation_calls.fetch_add(1, Ordering::Relaxed);
            self.allocated_bytes
                .fetch_add(layout.size(), Ordering::Relaxed);
            self.add_live_bytes(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: Delegating the caller-provided layout to System preserves the
        // GlobalAlloc contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if self.enabled.load(Ordering::Relaxed) && !pointer.is_null() {
            self.allocation_calls.fetch_add(1, Ordering::Relaxed);
            self.allocated_bytes
                .fetch_add(layout.size(), Ordering::Relaxed);
            self.add_live_bytes(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if self.enabled.load(Ordering::Relaxed) {
            self.deallocation_calls.fetch_add(1, Ordering::Relaxed);
            self.deallocated_bytes
                .fetch_add(layout.size(), Ordering::Relaxed);
            self.subtract_live_bytes(layout.size());
        }
        // SAFETY: System receives the exact pointer and layout supplied by its
        // corresponding allocation call.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old_layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: Delegating the original pointer/layout and requested size to
        // System preserves the GlobalAlloc contract.
        let new_pointer = unsafe { System.realloc(pointer, old_layout, new_size) };
        if self.enabled.load(Ordering::Relaxed) && !new_pointer.is_null() {
            self.reallocation_calls.fetch_add(1, Ordering::Relaxed);
            self.allocated_bytes.fetch_add(new_size, Ordering::Relaxed);
            self.deallocated_bytes
                .fetch_add(old_layout.size(), Ordering::Relaxed);
            if new_size >= old_layout.size() {
                self.add_live_bytes(new_size - old_layout.size());
            } else {
                self.subtract_live_bytes(old_layout.size() - new_size);
            }
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

#[derive(Debug, Clone, Copy)]
struct AllocationStats {
    allocation_calls: usize,
    reallocation_calls: usize,
    deallocation_calls: usize,
    allocated_bytes: usize,
    deallocated_bytes: usize,
    live_bytes: usize,
    peak_live_bytes: usize,
}

impl AllocationStats {
    fn as_json(self) -> Value {
        json!({
            "allocation_calls": self.allocation_calls,
            "reallocation_calls": self.reallocation_calls,
            "deallocation_calls": self.deallocation_calls,
            "allocated_bytes": self.allocated_bytes,
            "deallocated_bytes": self.deallocated_bytes,
            "live_bytes": self.live_bytes,
            "peak_live_bytes": self.peak_live_bytes,
        })
    }
}

fn scheme() -> Scheme {
    SchemeConfig::default()
        .build_scheme()
        .expect("default benchmark scheme is valid")
}

fn anchor_at(index: &FileIndex<'_>, scheme: Scheme, line: usize) -> String {
    scheme
        .anchor_at(index, line - 1)
        .expect("benchmark line is present")
        .render()
}

fn current_edit_fixture(content: &str, count: usize) -> Vec<HashlineOp> {
    let index = FileIndex::new(content);
    let lines = if count == 1 {
        vec![25_000usize]
    } else {
        (1..=count).map(|item| item * 6_000).collect()
    };
    lines
        .into_iter()
        .enumerate()
        .map(|(item, line)| HashlineOp::Replace {
            anchor: anchor_at(&index, scheme(), line),
            end_anchor: None,
            content: format!("RESOURCE REPLACEMENT {item}"),
        })
        .collect()
}

fn grep_input(pattern: &str, path: Option<String>) -> GrepRequest {
    GrepRequest {
        pattern: pattern.to_owned(),
        path,
        glob: Some("*.rs".to_owned()),
        ignore_case: false,
        after_context: None,
        before_context: None,
        context: None,
        max_matches: 200,
        output_mode: GrepOutputMode::Content,
    }
}

/// Build the probe runtime outside any allocation-counted region.
fn probe_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build probe runtime")
}

/// Drive the wired `run_read` through every page of `path`, returning the
/// total rendered bytes across the pagination chain.
async fn read_all_pages(workspace: &Workspace, path: &str) -> Result<usize> {
    let mut request = ReadRequest {
        path: path.to_owned(),
        limit: 2_000,
        cursor: None,
    };
    let mut total = 0usize;
    loop {
        let outcome = run_read(workspace, &request).await;
        if outcome.is_error {
            bail!("wired read failed: {}", outcome.text);
        }
        total += outcome.text.len();
        let footer = outcome
            .text
            .rfind('\n')
            .map_or(outcome.text.as_str(), |index| &outcome.text[index + 1..]);
        let Some(rest) = footer.strip_prefix("[hashline next snapshot=") else {
            return Ok(total);
        };
        let (snapshot, rest) = rest
            .split_once(' ')
            .context("cursor footer separates snapshot and position")?;
        let position = rest
            .strip_prefix("position=")
            .and_then(|token| token.strip_suffix(']'))
            .context("cursor footer carries a position token")?;
        request.cursor = Some(PageCursor {
            snapshot: snapshot.parse().context("parse cursor snapshot id")?,
            next: position.parse().context("parse cursor position")?,
        });
    }
}

/// One-line wired replace of line 25,000 against the exact corpus snapshot.
fn single_replace_request(content: &str, path: &str) -> Result<EditRequest> {
    let snapshot = Snapshot::from_bytes(content.as_bytes().to_vec())
        .context("build resource corpus snapshot")?
        .id();
    let offsets = phase0_workloads::offsets_u64(content);
    let start = Position::new(25_000, offsets[24_999]).context("start boundary")?;
    let end = Position::new(25_001, offsets[25_000]).context("end boundary")?;
    Ok(EditRequest {
        file_path: path.to_owned(),
        snapshot,
        edits: vec![EditOperation::replace(
            start,
            end,
            "RESOURCE REPLACEMENT 0\n".to_owned(),
        )],
    })
}

fn build_grep_fixture() -> Result<tempfile::TempDir> {
    let directory = tempfile::TempDir::new().context("create grep fixture root")?;
    for file_index in 0..GREP_FILE_COUNT {
        let path = directory
            .path()
            .join(format!("dir_{}", file_index / 100))
            .join(format!("sub_{}", (file_index / 10) % 10))
            .join(format!("file_{}.rs", file_index % 10));
        let parent = path.parent().context("grep fixture file has a parent")?;
        std::fs::create_dir_all(parent).context("create grep fixture directory")?;
        let mut content = phase0_workloads::generate_corpus(
            GREP_LINES_PER_FILE,
            0x1000_u32.wrapping_add(file_index as u32),
        );
        if file_index == GREP_FILE_COUNT / 2 {
            content.push_str(GREP_RARE_TOKEN);
            content.push('\n');
        }
        std::fs::write(path, content).context("write grep fixture")?;
    }
    Ok(directory)
}

fn emit_json(value: &Value) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer_pretty(&mut lock, value).context("serialize probe result")?;
    lock.write_all(b"\n").context("terminate probe result")?;
    Ok(())
}

fn measure_allocation<T>(operation: impl FnOnce() -> T) -> (T, AllocationStats) {
    ALLOCATOR.begin();
    let result = operation();
    let statistics = ALLOCATOR.finish();
    (result, statistics)
}

fn measure_named_scenario(scenario: &str, optional_path: Option<&Path>) -> Result<Value> {
    match scenario {
        "wired_read_full_10k" => {
            let directory = tempfile::TempDir::new().context("create wired read fixture root")?;
            std::fs::write(
                directory.path().join("corpus.rs"),
                phase0_workloads::generate_corpus(10_000, 0xA11C_E000),
            )
            .context("write wired read corpus")?;
            let workspace = Workspace::new(directory.path().to_path_buf(), false);
            let runtime = probe_runtime()?;
            let (total, allocations) =
                measure_allocation(|| runtime.block_on(read_all_pages(&workspace, "corpus.rs")));
            Ok(resource_json(scenario, total?, allocations))
        }
        "wired_edit_single_op_50k" => {
            let directory = tempfile::TempDir::new().context("create wired edit fixture root")?;
            let content = phase0_workloads::generate_corpus(50_000, 0xED17_0001);
            std::fs::write(directory.path().join("corpus.rs"), &content)
                .context("write wired edit corpus")?;
            let workspace = Workspace::new(directory.path().to_path_buf(), false);
            let request = single_replace_request(&content, "corpus.rs")?;
            let runtime = probe_runtime()?;
            let (outcome, allocations) =
                measure_allocation(|| runtime.block_on(run_edit(&workspace, &request)));
            if outcome.is_error {
                bail!("wired edit failed: {}", outcome.text);
            }
            Ok(resource_json(scenario, outcome.text.len(), allocations))
        }
        "tree_grep_base" => {
            let fixture = build_grep_fixture()?;
            let workspace = Workspace::new(fixture.path().to_path_buf(), false);
            let input = grep_input(GREP_COMMON_TOKEN, None);
            let (output, allocations) = measure_allocation(|| run_grep(&workspace, &input));
            Ok(resource_json(scenario, output.text.len(), allocations))
        }
        "real_tree_grep_base" => {
            let root = optional_path.context("real_tree_grep_base requires a repository path")?;
            let workspace = Workspace::new(root.to_path_buf(), false);
            let input = grep_input("pub ", None);
            let (output, allocations) = measure_allocation(|| run_grep(&workspace, &input));
            Ok(resource_json(scenario, output.text.len(), allocations))
        }
        _ => bail!("unknown resource scenario: {scenario}"),
    }
}

fn resource_json(scenario: &str, output_bytes: usize, allocations: AllocationStats) -> Value {
    json!({
        "schema_version": 1,
        "scenario": scenario,
        "output_bytes": output_bytes,
        "allocations": allocations.as_json(),
    })
}

#[cfg(target_os = "macos")]
fn configure_cold_descriptor(file: &File) -> Result<&'static str> {
    // SAFETY: fcntl receives a valid open descriptor and the documented
    // F_NOCACHE integer argument. It does not retain the descriptor.
    let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
    if status == -1 {
        return Err(std::io::Error::last_os_error()).context("set macOS F_NOCACHE");
    }
    Ok("fcntl(F_NOCACHE)=1 on timed descriptor")
}

#[cfg(target_os = "linux")]
fn configure_cold_descriptor(file: &File) -> Result<&'static str> {
    // SAFETY: posix_fadvise receives a valid descriptor and a whole-file range.
    // It does not retain the descriptor.
    let status = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status))
            .context("set Linux POSIX_FADV_DONTNEED");
    }
    Ok("posix_fadvise(POSIX_FADV_DONTNEED) before timed read")
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn configure_cold_descriptor(_file: &File) -> Result<&'static str> {
    bail!("cold filesystem measurement is unsupported on this operating system")
}

fn filesystem_probe(variant: &str, cache_state: &str, path: &Path) -> Result<Value> {
    if cache_state == "warm" {
        black_box(std::fs::read(path).context("warm page cache")?);
    }

    let mut file = File::open(path).context("open filesystem probe corpus")?;
    let cache_policy = match cache_state {
        "warm" => "pre-read entire file before timed descriptor",
        "cold" => configure_cold_descriptor(&file)?,
        _ => bail!("cache state must be cold or warm"),
    };

    let start = Instant::now();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .context("read filesystem probe corpus")?;
    let content = std::str::from_utf8(&bytes).context("probe corpus is UTF-8")?;
    let output_bytes = match variant {
        "base" => format_hashline_content(content, None, None, scheme()).len(),
        "candidate" => phase0_workloads::versioned_render_all(content).len(),
        _ => bail!("filesystem variant must be base or candidate"),
    };
    let elapsed = start.elapsed();

    Ok(json!({
        "schema_version": 1,
        "variant": variant,
        "cache_state": cache_state,
        "cache_policy": cache_policy,
        "input_bytes": bytes.len(),
        "output_bytes": output_bytes,
        "elapsed_ns": elapsed.as_nanos(),
    }))
}

#[inline(never)]
fn profile_full_read_once(content: &str) -> usize {
    format_hashline_content(content, None, None, scheme()).len()
}

#[inline(never)]
fn profile_edit_once(content: &str, operations: &[HashlineOp]) -> usize {
    apply_edits(content, operations, scheme())
        .new_content
        .expect("profile edit succeeds")
        .len()
}

#[inline(never)]
fn profile_grep_once(workspace: &Workspace, input: &GrepRequest) -> usize {
    run_grep(workspace, input).text.len()
}

fn profile_scenario(scenario: &str, seconds: u64) -> Result<Value> {
    let duration = Duration::from_secs(seconds);
    let deadline = Instant::now() + duration;
    let mut iterations = 0u64;

    match scenario {
        "full_read" => {
            let content = phase0_workloads::generate_corpus(10_000, 0xA11C_E000);
            while Instant::now() < deadline {
                black_box(profile_full_read_once(&content));
                iterations += 1;
            }
        }
        "edit" => {
            let content = phase0_workloads::generate_corpus(50_000, 0xED17_0001);
            let operations = current_edit_fixture(&content, 1);
            while Instant::now() < deadline {
                black_box(profile_edit_once(&content, &operations));
                iterations += 1;
            }
        }
        "rare_grep" | "common_grep" => {
            let fixture = build_grep_fixture()?;
            let workspace = Workspace::new(fixture.path().to_path_buf(), false);
            let pattern = if scenario == "rare_grep" {
                GREP_RARE_TOKEN
            } else {
                GREP_COMMON_TOKEN
            };
            let input = grep_input(pattern, None);
            while Instant::now() < deadline {
                black_box(profile_grep_once(&workspace, &input));
                iterations += 1;
            }
        }
        _ => bail!("unknown profile scenario: {scenario}"),
    }

    Ok(json!({
        "schema_version": 1,
        "scenario": scenario,
        "requested_seconds": seconds,
        "iterations": iterations,
    }))
}

fn self_test() -> Result<Value> {
    let content = "alpha\nbeta\ngamma\n";
    let narrow = phase0_workloads::offsets_u32(content);
    let wide = phase0_workloads::offsets_u64(content);
    if narrow
        .iter()
        .map(|&offset| u64::from(offset))
        .ne(wide.iter().copied())
    {
        bail!("u32 and u64 offsets diverged");
    }

    let selected = phase0_workloads::sparse_select(content, 1, 2);
    if selected.starts != wide[1..3] || selected.end != wide[3] {
        bail!("sparse selector diverged from the full offset table");
    }

    let edits = phase0_workloads::replacement_edits(content, &[2]);
    if phase0_workloads::apply_byte_edits(content, &edits)
        != "alpha\nREPLACED POSITIONAL LINE 0\ngamma\n"
    {
        bail!("byte splice output mismatch");
    }

    Ok(json!({
        "schema_version": 1,
        "status": "pass",
        "checks": [
            "u32_u64_offset_equivalence",
            "sparse_full_offset_equivalence",
            "byte_splice_preservation"
        ],
    }))
}

fn parse_seed(value: &str) -> Result<u32> {
    let trimmed = value.strip_prefix("0x").unwrap_or(value);
    u32::from_str_radix(trimmed, 16).with_context(|| format!("parse hexadecimal seed {value}"))
}

fn write_corpus(lines: &str, seed: &str, path: &Path) -> Result<Value> {
    let line_count = lines.parse::<usize>().context("parse corpus line count")?;
    let seed = parse_seed(seed)?;
    let content = phase0_workloads::generate_corpus(line_count, seed);
    std::fs::write(path, &content).with_context(|| format!("write corpus {}", path.display()))?;
    Ok(json!({
        "schema_version": 1,
        "path": path,
        "requested_lines": line_count,
        "logical_lines": phase0_workloads::logical_line_count(&content),
        "bytes": content.len(),
        "seed": format!("0x{seed:08x}"),
    }))
}

fn required_argument<'a>(arguments: &'a [String], index: usize, name: &str) -> Result<&'a str> {
    arguments
        .get(index)
        .map(String::as_str)
        .with_context(|| format!("missing {name} argument"))
}

fn run() -> Result<()> {
    let arguments = std::env::args().collect::<Vec<_>>();
    if arguments.len() == 1 {
        return emit_json(&self_test()?);
    }
    let command = required_argument(&arguments, 1, "command")?;
    let value = match command {
        "self-test" => self_test()?,
        "corpus" => {
            let lines = required_argument(&arguments, 2, "lines")?;
            let seed = required_argument(&arguments, 3, "seed")?;
            let path = PathBuf::from(required_argument(&arguments, 4, "output path")?);
            write_corpus(lines, seed, &path)?
        }
        "measure" => {
            let scenario = required_argument(&arguments, 2, "scenario")?;
            let path = arguments.get(3).map(PathBuf::from);
            measure_named_scenario(scenario, path.as_deref())?
        }
        "filesystem" => {
            let variant = required_argument(&arguments, 2, "variant")?;
            let cache_state = required_argument(&arguments, 3, "cache state")?;
            let path = PathBuf::from(required_argument(&arguments, 4, "corpus path")?);
            filesystem_probe(variant, cache_state, &path)?
        }
        "profile" => {
            let scenario = required_argument(&arguments, 2, "scenario")?;
            let seconds = required_argument(&arguments, 3, "seconds")?
                .parse::<u64>()
                .context("parse profile duration")?;
            profile_scenario(scenario, seconds)?
        }
        _ => bail!("unknown command: {command}"),
    };
    emit_json(&value)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("phase0 resource probe failed: {error:#}");
        std::process::exit(1);
    }
}
