#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| scion_sim::fuzz::history_page(data));
