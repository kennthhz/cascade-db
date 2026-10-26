//! Compiles the vendored libpg_query C sources (PostgreSQL 17 parser) into
//! a static library. The recipe mirrors libpg_query's own Makefile /
//! pganalyze's pg_query crate build: `src/*.c` + `src/postgres/*.c` +
//! vendored protobuf-c/xxhash + the generated protobuf serializer, with
//! the include roots below. Parser sources are pre-generated upstream, so
//! only a C compiler is needed — no bison, flex, protoc, or libclang.
//!
//! `links = "pg_query"` in the manifest guarantees exactly one copy of
//! this C library in any dependency graph.

use std::path::{Path, PathBuf};

fn main() {
    let root = Path::new("libpg_query");
    println!("cargo:rerun-if-changed=libpg_query");

    let mut build = cc::Build::new();
    for dir in ["src", "src/postgres"] {
        for entry in c_files(dir) {
            build.file(entry);
        }
    }
    build
        .file(root.join("vendor/protobuf-c/protobuf-c.c"))
        .file(root.join("vendor/xxhash/xxhash.c"))
        .file(root.join("protobuf/pg_query.pb-c.c"))
        .include(root)
        .include(root.join("vendor"))
        .include(root.join("src/postgres/include"))
        .include(root.join("src/include"))
        .warnings(false); // the C tree is upstream's; their warning bar, not ours
    // libpg_query's own debug builds assert internally; mirror that choice.
    if std::env::var("PROFILE").as_deref() == Ok("debug") {
        build.define("USE_ASSERT_CHECKING", None);
    }
    build.compile("pg_query");
}

/// Every `.c` file under `libpg_query/<dir>`, sorted (deterministic object
/// order — same lib every build).
fn c_files(dir: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(Path::new("libpg_query").join(dir))
        .unwrap_or_else(|e| panic!("vendored dir libpg_query/{dir}: {e}"))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "c"))
        .collect();
    out.sort();
    out
}
