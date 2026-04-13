/// A test helper binary that ignores all arguments and sleeps forever.
/// Used to test timeout handling in the runner.
fn main() {
    std::thread::sleep(std::time::Duration::from_secs(999));
}
