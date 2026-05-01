//! Test helper that writes its CLI arguments to a JSON file and exits 0.
//!
//! Used by integration tests to verify that the runner injects the expected
//! arguments (e.g. `--peers`) when spawning a variant. The output file path
//! is read from the `ARG_ECHO_OUT` environment variable.

use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out_path =
        std::env::var("ARG_ECHO_OUT").expect("ARG_ECHO_OUT environment variable must be set");
    let mut f = std::fs::File::create(&out_path).expect("create output file");
    let json = serde_json::to_string(&args).expect("serialize args");
    f.write_all(json.as_bytes()).expect("write args");
}
