//! The self-replacing stub binary (feature `exe`). A packer injects a
//! zstd-compressed payload executable into a copy of THIS binary; on first run
//! the copy decompresses the payload, writes it back to disk FS-compressed,
//! atomically replaces itself, and execs it. Every later run is the plain
//! materialized executable — this stub is gone.
//!
//! A bare, unpacked `decmpfs-stub` carries no payload, so `self_replace_and_exec`
//! returns `Ok(false)` and there is nothing to run: it exits non-zero with a
//! diagnostic rather than pretend-succeeding.

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match decmpfs::exe::self_replace_and_exec(&argv) {
        // Unix: exec replaces the process image, so Ok(true) never actually
        // returns here. Windows spawns the materialized sibling and returns true.
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "decmpfs-stub: no packed payload in this binary — pack one in with \
         decmpfs::exe::pack_executable_with_stub before running it."
            );
            std::process::exit(2);
        }
        Err(e) => {
            eprintln!("decmpfs-stub: {e}");
            std::process::exit(1);
        }
    }
}
