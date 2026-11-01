#![no_main]

// The parsers, built from Granary's own sources.
#[allow(dead_code)]
#[path = "../../src/gomod.rs"]
mod gomod;
#[allow(dead_code)]
#[path = "../../src/gomodfile.rs"]
mod gomodfile;

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    gomodfile::fuzz::go_line(data);
});
