// Copyright 2026-Present Datadog, Inc. https://www.datadoghq.com/
// SPDX-License-Identifier: Apache-2.0

use std::fmt::Write;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = manifest.parent().unwrap();
    let mut generated = String::new();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (constant, dir) in [
        ("CORE_SOURCES", "dd-test-core/src"),
        ("HARNESS_SOURCES", "dd-test-harness/src"),
    ] {
        let root = crates.join(dir);
        println!("cargo:rerun-if-changed={}", root.display());
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        writeln!(generated, "pub const {constant}: &[(&str, &str)] = &[").unwrap();
        for file in files {
            let relative = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            for byte in relative.bytes().chain(std::fs::read(&file).unwrap()) {
                hash = (hash ^ byte as u64).wrapping_mul(0x100_0000_01b3);
            }
            writeln!(
                generated,
                "    ({relative:?}, include_str!({:?})),",
                file.display().to_string()
            )
            .unwrap();
        }
        writeln!(generated, "];").unwrap();
    }
    writeln!(generated, "pub const SOURCE_HASH: u64 = {hash:#x};").unwrap();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("sources.rs");
    std::fs::write(out, generated).unwrap();
}
