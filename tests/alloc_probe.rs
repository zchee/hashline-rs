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
//
//! Allocation-shape probe for the wired edit and read paths.
//!
//! Its own integration-test binary so a counting `#[global_allocator]` can
//! wrap the system allocator without touching any other target. It records
//! measurements (printed with `--nocapture`) rather than asserting absolute
//! numbers, so allocator or dependency drift cannot turn it into a flake;
//! the optimization plan reads the printed ratios as acceptance evidence.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    fmt::Write as _,
    sync::atomic::{AtomicU64, Ordering},
};

use hashline::{
    edit,
    protocol::{EditOperation, EditRequest, Position, ReadRequest, SnapshotId},
    read,
    util::Workspace,
};

struct CountingAllocator;

static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Count only the grown portion so a Vec doubling series is not
        // double-counted against the copy budget.
        if new_size > layout.size() {
            ALLOCATED_BYTES.fetch_add((new_size - layout.size()) as u64, Ordering::Relaxed);
        }
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn corpus(lines: usize) -> String {
    let mut out = String::with_capacity(lines * 28);
    for i in 0..lines {
        let _ = writeln!(out, "let value_{i} = compute(state, {i});");
    }
    out
}

fn snapshot_counters() -> (u64, u64) {
    (
        ALLOCATED_BYTES.load(Ordering::Relaxed),
        ALLOC_CALLS.load(Ordering::Relaxed),
    )
}

/// Measure one closure's allocation delta on a current-thread runtime.
///
/// Single-threaded so no concurrent test pollutes the counters; this binary
/// contains only the probe tests and harness threads are quiesced by running
/// the probes serially inside one test.
fn measure<F>(runtime: &tokio::runtime::Runtime, work: F) -> (u64, u64)
where
    F: std::future::Future<Output = ()>,
{
    let (bytes_before, calls_before) = snapshot_counters();
    runtime.block_on(work);
    let (bytes_after, calls_after) = snapshot_counters();
    (bytes_after - bytes_before, calls_after - calls_before)
}

#[test]
fn alloc_shape_wired_edit_and_read() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let workspace = Workspace::new(tmp.path().to_path_buf(), false);

    let content = corpus(50_000);
    let file_bytes = content.len() as u64;
    std::fs::write(tmp.path().join("edit50k.rs"), &content).expect("write fixture");

    // Warm the process cache and harvest the snapshot + splice positions.
    let page = runtime
        .block_on(read::run(
            &workspace,
            &ReadRequest {
                path: "edit50k.rs".into(),
                limit: 3,
                cursor: None,
                start_line: None,
            },
        ))
        .expect("warm read");
    let header = page.lines().next().expect("header");
    let snapshot: SnapshotId = header
        .split_whitespace()
        .find_map(|part| part.strip_prefix("snapshot="))
        .expect("snapshot field")
        .parse()
        .expect("snapshot id");
    let start = page
        .lines()
        .find(|line| line.starts_with("2@"))
        .and_then(|line| line.split('|').next())
        .expect("line 2 position")
        .parse::<Position>()
        .expect("position");
    let end = page
        .lines()
        .find(|line| line.starts_with("3@"))
        .and_then(|line| line.split('|').next())
        .expect("line 3 position")
        .parse::<Position>()
        .expect("position");

    let request = EditRequest {
        file_path: "edit50k.rs".into(),
        snapshot,
        edits: vec![EditOperation::replace(
            start,
            end,
            "let value_1 = replaced(state, 1);\n".into(),
        )],
    };
    let (edit_bytes, edit_calls) = measure(&runtime, async {
        edit::run(&workspace, &request).await.expect("wired edit");
    });

    let content10k = corpus(10_000);
    std::fs::write(tmp.path().join("read10k.rs"), &content10k).expect("write fixture");
    let read10k_bytes = content10k.len() as u64;
    let (read_bytes, read_calls) = measure(&runtime, async {
        let mut cursor = None;
        loop {
            let text = read::run(
                &workspace,
                &ReadRequest {
                    path: "read10k.rs".into(),
                    limit: 2000,
                    cursor: cursor.take(),
                    start_line: None,
                },
            )
            .await
            .expect("wired read page");
            match text.lines().last().and_then(|line| {
                line.strip_prefix("[hashline next snapshot=")
                    .map(str::to_owned)
            }) {
                Some(rest) => {
                    let mut parts = rest.trim_end_matches(']').split(" position=");
                    let snapshot = parts.next().expect("cursor snapshot").parse().expect("id");
                    let next = parts.next().expect("cursor position").parse().expect("pos");
                    cursor = Some(hashline::protocol::PageCursor { snapshot, next });
                }
                None => break,
            }
        }
    });

    println!(
        "ALLOC_PROBE edit50k: file_bytes={file_bytes} allocated={edit_bytes} \
         ratio={:.2} calls={edit_calls}",
        edit_bytes as f64 / file_bytes as f64
    );
    println!(
        "ALLOC_PROBE read10k_full: file_bytes={read10k_bytes} allocated={read_bytes} \
         ratio={:.2} calls={read_calls}",
        read_bytes as f64 / read10k_bytes as f64
    );

    // Sanity floor only — the real numbers are read by the optimization plan.
    assert!(edit_bytes > 0 && read_bytes > 0);
}
