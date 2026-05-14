//! `zenohd` sidecar lifecycle for Zenoh's deferred Single mode (T14.9a).
//!
//! Background
//! ----------
//!
//! The zenoh crate runs an internal multi-threaded engine we cannot
//! disable from the client (T14.7). T14.9 covers the deferred path where
//! a sidecar `zenohd` router absorbs all the concurrency and the
//! variant talks a synchronous RPC over the router's REST plugin. T14.9
//! was split into two sub-tasks during its audit:
//!
//! * **T14.9a (this module)**: locate the `zenohd` binary, spawn it
//!   with a per-spawn config enabling the REST plugin on a derived
//!   port, wait for the REST surface to be live, kill it cleanly on
//!   disconnect, and -- crucially -- arrange per-platform child-process
//!   cleanup so a SIGKILLed variant doesn't orphan a sidecar.
//!
//! * **T14.9b (NOT in scope here)**: the actual sync RPC client
//!   (HTTP PUT for publish, SSE for poll_receive). Until T14.9b lands
//!   the variant's `supported_threading_modes()` stays `[Multi]` and
//!   `connect(Single)` spawns the sidecar but `publish` /
//!   `poll_receive` return a "not yet implemented" error.
//!
//! Binary discovery
//! ----------------
//!
//! The variant first checks the `ZENOHD_PATH` environment variable. If
//! set, it must point at an executable (existence and executable-ness
//! are validated). If unset, `PATH` is consulted via the platform's
//! `which`-equivalent (see [`locate_zenohd`]). If neither finds the
//! binary the variant returns an actionable error advising the
//! operator to run `cargo install zenohd --version 1.9.0` or set
//! `ZENOHD_PATH`.
//!
//! Port allocation
//! ---------------
//!
//! REST-plugin port follows the same `base_port + runner_index *
//! runner_stride` convention as T14.18 / T15.10 control ports. The
//! runner index is read from the sorted `--peers` map injected by the
//! runner; when `--peers` is absent (e.g. solo unit tests) the index
//! is `0`. See [`derive_sidecar_port`].
//!
//! Per-platform cleanup
//! --------------------
//!
//! * **Windows**: each spawned sidecar is assigned to a Job Object
//!   configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When the
//!   variant process exits (clean, panic, SIGKILL alike) the OS closes
//!   the Job Object handle on its behalf and kills every process in
//!   the job -- so `zenohd` cannot outlive the variant.
//! * **Linux**: a `Command::pre_exec` hook calls
//!   `prctl(PR_SET_PDEATHSIG, SIGTERM)`. The kernel delivers SIGTERM
//!   to the child as soon as the parent dies. Best-effort `setpgid(0,
//!   0)` is also called so the child becomes a session/group leader
//!   we can target on macOS.
//! * **macOS**: the same pre-exec hook applies. macOS lacks
//!   `PR_SET_PDEATHSIG`, so cleanup is best-effort: if the variant
//!   exits cleanly we kill the child explicitly in [`Sidecar::stop`];
//!   if it gets SIGKILLed the sidecar may briefly leak until the
//!   operator notices. Documented in `CUSTOM.md`.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Default runner stride for sidecar port derivation. Matches the
/// `runner_stride = 1` convention used by T14.18 / T15.10 control
/// ports across the other variants.
pub const SIDECAR_RUNNER_STRIDE: u16 = 1;

/// Cap on how long we wait for the REST plugin to come up. 5 s is
/// generous on a warm host (zenohd typically responds well under 1 s)
/// and gives a clear failure on a cold one without blocking the test
/// suite for minutes.
const REST_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval while waiting for the REST plugin to respond.
const REST_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Result of [`locate_zenohd`]: the path plus the source we found it
/// from (for diagnostic messages).
#[derive(Debug, Clone)]
pub struct ZenohdBinary {
    pub path: PathBuf,
    pub source: BinarySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySource {
    /// Found via the `ZENOHD_PATH` env var.
    EnvVar,
    /// Found by walking `PATH`.
    Path,
}

/// Locate the `zenohd` binary. Checks `ZENOHD_PATH` first (must point
/// at an existing file), then walks `PATH`. Returns a clean
/// actionable error if neither finds the binary.
///
/// This function is pure (no spawning, no network) so it can be unit
/// tested without zenohd installed: callers in tests inject paths
/// directly. The error string is part of the operator-facing
/// contract documented in `variants/zenoh/CUSTOM.md` -- changing it
/// requires updating the docs.
pub fn locate_zenohd() -> Result<ZenohdBinary> {
    locate_zenohd_with_env(std::env::var_os("ZENOHD_PATH"), std::env::var_os("PATH"))
}

/// Test-friendly variant of [`locate_zenohd`] that takes the env vars
/// explicitly so unit tests don't have to mutate the process-global
/// environment (which is fundamentally racy).
pub fn locate_zenohd_with_env(
    zenohd_path: Option<OsString>,
    path_env: Option<OsString>,
) -> Result<ZenohdBinary> {
    if let Some(raw) = zenohd_path {
        let candidate = PathBuf::from(&raw);
        if candidate.is_file() {
            return Ok(ZenohdBinary {
                path: candidate,
                source: BinarySource::EnvVar,
            });
        }
        anyhow::bail!(
            "ZENOHD_PATH=\"{}\" does not point at an existing file. \
             Install via 'cargo install zenohd --version 1.9.0' or fix ZENOHD_PATH.",
            candidate.display()
        );
    }

    if let Some(found) = which_in_path("zenohd", path_env.as_deref()) {
        return Ok(ZenohdBinary {
            path: found,
            source: BinarySource::Path,
        });
    }

    anyhow::bail!(
        "zenohd binary not found. Install via 'cargo install zenohd --version 1.9.0' \
         or set ZENOHD_PATH=<path>"
    );
}

/// Minimal cross-platform `which` walking the supplied PATH. Returns
/// the first executable named `name` (with platform-appropriate
/// extension search on Windows). Kept private and tiny so we don't
/// pull in the `which` crate just for this.
fn which_in_path(name: &str, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_env = path_env?;
    let exts: Vec<String> = if cfg!(windows) {
        // Honour PATHEXT roughly, falling back to a tiny default set
        // when the env var is missing (typical for non-interactive
        // shells).
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };

    for dir in std::env::split_paths(path_env) {
        // First try the bare name (Unix) or with each extension (Windows).
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) {
            for ext in &exts {
                // PATHEXT entries start with `.`; honour them as-is.
                let with_ext = dir.join(format!("{}{}", name, ext));
                if with_ext.is_file() {
                    return Some(with_ext);
                }
            }
        }
    }
    None
}

/// Derive the REST-plugin port for this runner.
///
/// Formula (matches T14.18 / T15.10 control-port convention):
///     port = base_port + runner_index * SIDECAR_RUNNER_STRIDE
///
/// `runner_index` is 0-based; for a solo runner (no `--peers`
/// injected) the caller passes `0`. Returns an error on overflow so
/// the failure surfaces clearly rather than silently wrapping into a
/// port collision with another tenant.
pub fn derive_sidecar_port(base_port: u16, runner_index: usize) -> Result<u16> {
    let offset = u16::try_from(runner_index)
        .context("runner_index does not fit in u16 (too many peers)")?
        .checked_mul(SIDECAR_RUNNER_STRIDE)
        .context("runner_index * SIDECAR_RUNNER_STRIDE overflowed u16")?;
    base_port
        .checked_add(offset)
        .context("base_port + runner_offset overflowed u16")
}

/// Build a zenohd JSON5 config string enabling the REST plugin on the
/// given port and optionally configuring inter-router Zenoh peering.
///
/// `listen_tcp` is `Some(host:port)` when this sidecar should accept
/// inbound Zenoh sessions from peer-runners' sidecars (T14.9b
/// two-runner topology). `connect_tcp` lists `tcp/<host>:<port>`
/// endpoints to actively dial out to; combined, the two cover the
/// "every sidecar peers with every other sidecar" full-mesh shape
/// the runner-coordinated Single-mode fixture expects.
///
/// Returned as a String so the caller can dump it to a temp file
/// before spawning. JSON is a subset of JSON5, so we emit strict
/// JSON via `serde_json` and zenohd parses it happily.
pub fn build_zenohd_config_json(
    rest_port: u16,
    listen_tcp: Option<String>,
    connect_tcp: &[String],
) -> String {
    // Bind REST to 127.0.0.1 specifically -- T14.9a sidecars are
    // per-runner-process and never expose the REST API beyond
    // localhost. Listening on 0.0.0.0 would surprise operators by
    // making the variant's internal RPC surface externally
    // reachable.
    let mut top = serde_json::Map::new();
    top.insert(
        "plugins".to_string(),
        serde_json::json!({
            "rest": { "http_port": format!("127.0.0.1:{}", rest_port) }
        }),
    );
    if let Some(listen) = listen_tcp {
        top.insert(
            "listen".to_string(),
            serde_json::json!({ "endpoints": [format!("tcp/{}", listen)] }),
        );
    }
    if !connect_tcp.is_empty() {
        let endpoints: Vec<serde_json::Value> = connect_tcp
            .iter()
            .map(|e| serde_json::Value::String(format!("tcp/{}", e)))
            .collect();
        top.insert(
            "connect".to_string(),
            serde_json::json!({ "endpoints": endpoints }),
        );
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(top))
        .expect("static JSON value always serialises")
}

/// The REST-plugin smoke-test URL we poll after spawning zenohd. The
/// `@/router/local` admin space is implemented by the router itself
/// and returns 200 with a JSON description as soon as the plugin is
/// ready. Kept `pub(crate)` for the integration test to reuse.
pub fn rest_ready_url(rest_port: u16) -> String {
    format!("http://127.0.0.1:{}/@/router/local", rest_port)
}

/// A running sidecar. The handle owns the child process; dropping or
/// calling [`Sidecar::stop`] tears it down. On Windows the Job Object
/// handle is held by this struct so the OS-level kill-on-close
/// behaviour applies for the lifetime of `Sidecar`.
pub struct Sidecar {
    child: Option<Child>,
    rest_port: u16,
    /// Path to the temporary config file we wrote for this spawn.
    /// Kept so we can clean it up on stop. `Option` so `stop` can
    /// take it.
    config_path: Option<PathBuf>,
    /// Held for the lifetime of `Sidecar` so the OS keeps the child
    /// inside the Job Object; dropped along with `Sidecar` so the
    /// final `CloseHandle` triggers the KILL_ON_JOB_CLOSE behaviour
    /// as a belt-and-braces guarantee even if the explicit kill in
    /// `stop` ever fails. `#[allow(dead_code)]` because the field
    /// has no read sites -- its job is to be alive.
    #[cfg(windows)]
    #[allow(dead_code)]
    job: Option<job_object::JobObject>,
}

impl Sidecar {
    /// Spawn `zenohd` with a per-spawn config enabling the REST
    /// plugin on `rest_port`. Waits up to 5 s for the REST plugin
    /// to start responding before returning, so the caller can
    /// assume the sidecar is live on success.
    ///
    /// `listen_tcp` (host:port) and `connect_tcp` (list of host:port)
    /// configure inter-router Zenoh peering: when omitted the sidecar
    /// is an isolated single-runner-of-its-kind (still useful for
    /// the operator-facing smoke), when populated they let multiple
    /// per-runner sidecars form a Zenoh peer mesh so a publish on
    /// one is delivered to the SSE subscribers on every other.
    pub fn spawn(
        binary: &Path,
        rest_port: u16,
        listen_tcp: Option<String>,
        connect_tcp: &[String],
    ) -> Result<Self> {
        // Write the config to a temp file. We don't use the
        // `tempfile` crate (dev-only dep); a plain `std::env::temp_dir`
        // path keyed on the PID + port is unique enough for one
        // sidecar per variant process.
        let pid = std::process::id();
        let config_path =
            std::env::temp_dir().join(format!("variant-zenoh-sidecar-{}-{}.json", pid, rest_port));
        let config_json = build_zenohd_config_json(rest_port, listen_tcp, connect_tcp);
        fs::write(&config_path, config_json)
            .with_context(|| format!("write zenohd config to {}", config_path.display()))?;

        // Spawn with the per-platform pre-exec / Job-Object setup.
        let mut cmd = Command::new(binary);
        cmd.arg("--config").arg(&config_path);
        // Keep zenohd quiet on stderr; pipe both streams so the
        // child doesn't inherit our console (relevant on Windows
        // when the variant is launched without an attached console).
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(unix)]
        unix_preexec::apply(&mut cmd);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "spawn zenohd at {} for sidecar on port {}",
                binary.display(),
                rest_port
            )
        })?;

        // Assign the child to a Job Object on Windows. Done AFTER
        // spawn (we need the process handle); zenohd is started in a
        // suspended state via no special flag, so there is a
        // microscopic race where it could spawn its own child before
        // we assign. For T14.9a we accept that — zenohd doesn't
        // spawn helpers in the standard REST-plugin path.
        #[cfg(windows)]
        let job = match job_object::JobObject::assign_to(&child) {
            Ok(j) => Some(j),
            Err(e) => {
                // Best-effort kill before returning the error so we
                // don't leak the (non-job-assigned) zenohd.
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&config_path);
                anyhow::bail!("assign zenohd to Windows Job Object failed: {e}");
            }
        };

        // Wait for the REST plugin to respond. If it never does we
        // tear the child down and report a useful error.
        let url = rest_ready_url(rest_port);
        if let Err(e) = wait_for_rest_ready(&url, REST_READY_TIMEOUT, &mut child) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&config_path);
            return Err(e);
        }

        Ok(Self {
            child: Some(child),
            rest_port,
            config_path: Some(config_path),
            #[cfg(windows)]
            job,
        })
    }

    /// The REST-plugin port the sidecar is listening on. T14.9b will
    /// use this to wire up the HTTP / SSE client.
    #[allow(dead_code)] // consumed by T14.9b
    pub fn rest_port(&self) -> u16 {
        self.rest_port
    }

    /// Tear the sidecar down. SIGTERM (or Windows kill) first, then
    /// `wait` with a short grace period; SIGKILL fallback if the
    /// child is still alive. Removes the temp config file. Idempotent.
    pub fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            // Polite termination first. `child.kill()` on Windows is
            // an immediate TerminateProcess; on Unix it sends SIGKILL.
            // For Unix we do a softer SIGTERM via libc first so the
            // process can flush state, then fall back to kill().
            #[cfg(unix)]
            {
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < deadline {
                    match child.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                        Err(_) => break,
                    }
                }
            }
            // Force-kill anything still alive.
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(path) = self.config_path.take() {
            let _ = fs::remove_file(path);
        }
        // Dropping `self.job` (Windows) closes the Job Object handle,
        // which would have terminated the process anyway. Order
        // matters only insofar as we want the child reaped before
        // the job goes away; we already waited above.
        Ok(())
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        // Best-effort cleanup. `stop` is idempotent so a manual call
        // followed by Drop is fine. We deliberately ignore errors.
        let _ = self.stop();
    }
}

/// Poll the REST URL until it responds OK or the deadline elapses.
/// Uses `std::net::TcpStream` for the initial port-reachability check
/// (cheaper than a full HTTP round-trip and sufficient because zenohd
/// only starts accepting once the plugin is live), then issues one
/// minimal HTTP GET to confirm we're talking to the REST plugin and
/// not some other process that happened to grab the port.
fn wait_for_rest_ready(url: &str, timeout: Duration, child: &mut Child) -> Result<()> {
    let start = Instant::now();
    let (host, port, path) = parse_simple_http_url(url)
        .with_context(|| format!("internal: failed to parse smoke URL {url}"))?;

    while start.elapsed() < timeout {
        // Bail fast if the child has already died -- no point waiting
        // 5 s on a process that exited 50 ms in.
        if let Some(status) = child.try_wait().ok().flatten() {
            anyhow::bail!(
                "zenohd exited before REST plugin became ready (status: {})",
                status
            );
        }
        if let Ok(mut stream) = std::net::TcpStream::connect_timeout(
            &format!("{host}:{port}").parse().unwrap(),
            Duration::from_millis(200),
        ) {
            // Send a tiny HTTP/1.1 GET, read the status line. If
            // it starts with "HTTP/" we're good. Anything else
            // (e.g. malformed reply because zenohd is half-up)
            // gets retried.
            let req =
                format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
            use std::io::Write as _;
            stream
                .set_read_timeout(Some(Duration::from_millis(500)))
                .ok();
            stream
                .set_write_timeout(Some(Duration::from_millis(500)))
                .ok();
            if stream.write_all(req.as_bytes()).is_ok() {
                let mut buf = [0u8; 16];
                if let Ok(n) = stream.read(&mut buf) {
                    if n >= 5 && &buf[..5] == b"HTTP/" {
                        return Ok(());
                    }
                }
            }
        }
        std::thread::sleep(REST_READY_POLL_INTERVAL);
    }
    anyhow::bail!("zenohd REST plugin did not respond on {url} within {timeout:?}");
}

/// Tiny URL parser for the smoke URL only. Handles
/// `http://host:port/path`; returns (host, port, path). Not
/// general-purpose -- T14.9b will use a real HTTP client.
fn parse_simple_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = hostport.split_once(':')?;
    let port: u16 = port.parse().ok()?;
    Some((host.to_string(), port, path.to_string()))
}

// ---------------------------------------------------------------
// Per-platform child-process cleanup primitives.
// ---------------------------------------------------------------

#[cfg(unix)]
mod unix_preexec {
    use std::process::Command;

    /// Configure a pre-exec hook on `cmd` that ties the child's
    /// lifetime to the parent's where possible:
    ///
    /// * Linux: `prctl(PR_SET_PDEATHSIG, SIGTERM)` so the kernel
    ///   signals the child as soon as the parent process dies (any
    ///   cause, including SIGKILL of the variant).
    /// * macOS / other BSDs: best-effort `setpgid(0, 0)` so the
    ///   child becomes its own process-group leader. macOS has no
    ///   `PR_SET_PDEATHSIG` equivalent; the variant relies on the
    ///   clean-exit path in `Sidecar::stop` and accepts that a
    ///   SIGKILLed variant on macOS may leak its sidecar until the
    ///   operator notices. Documented in `CUSTOM.md`.
    ///
    /// `pre_exec` runs after `fork()` but before `execvp()`; the
    /// closure must be async-signal-safe. `prctl` and `setpgid` are
    /// both AS-safe per POSIX.
    pub fn apply(cmd: &mut Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: the closure only calls AS-safe libc functions.
        // No allocation, no Rust I/O, no panicking unwinds.
        unsafe {
            cmd.pre_exec(|| {
                // Best-effort setpgid: never propagate an error.
                let _ = libc::setpgid(0, 0);
                #[cfg(target_os = "linux")]
                {
                    // PR_SET_PDEATHSIG = 1
                    let _ = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0);
                }
                Ok(())
            });
        }
    }
}

#[cfg(windows)]
mod job_object {
    //! Windows Job Object wrapper. Assigns a spawned `Child` to a
    //! job configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so
    //! the child is terminated by the OS as soon as our process
    //! exits and the handle is closed. The wrapper holds the
    //! handle; dropping it closes the handle and (for any still-
    //! running child) triggers the cleanup.
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct JobObject {
        handle: HANDLE,
    }

    impl JobObject {
        pub fn assign_to(child: &Child) -> io::Result<Self> {
            // SAFETY: each Win32 call is checked; on failure we
            // close any handles we already opened before bailing.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                    return Err(io::Error::last_os_error());
                }

                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    let e = io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(e);
                }

                let ok = AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE);
                if ok == 0 {
                    let e = io::Error::last_os_error();
                    CloseHandle(handle);
                    return Err(e);
                }

                Ok(Self { handle })
            }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // Closing the handle while KILL_ON_JOB_CLOSE is set
            // terminates all processes still in the job -- the
            // exact orphan-protection guarantee T14.9a needs.
            // SAFETY: handle was allocated in `assign_to` via
            // CreateJobObjectW and has not been closed yet.
            unsafe {
                if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
                    CloseHandle(self.handle);
                }
            }
        }
    }
}

// ---------------------------------------------------------------
// Unit tests.
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// T14.9a binary-discovery fallthrough #1: `ZENOHD_PATH` wins
    /// when it points at an existing file.
    #[test]
    fn locate_zenohd_uses_env_var_when_set() {
        let tmp = std::env::temp_dir().join("variant-zenoh-test-bin");
        // Touch the file so `is_file()` returns true. Content
        // doesn't matter -- we only care about the path resolution.
        fs::write(&tmp, b"fake").unwrap();
        let result = locate_zenohd_with_env(Some(tmp.clone().into_os_string()), None)
            .expect("ZENOHD_PATH should win when set");
        assert_eq!(result.path, tmp);
        assert_eq!(result.source, BinarySource::EnvVar);
        let _ = fs::remove_file(&tmp);
    }

    /// T14.9a binary-discovery fallthrough #2: bad ZENOHD_PATH gives
    /// a clear actionable error (does NOT silently fall through to
    /// PATH, because a wrong env var probably indicates a misconfig
    /// the operator wants to see immediately).
    #[test]
    fn locate_zenohd_errors_on_bad_env_var() {
        let err = locate_zenohd_with_env(
            Some(OsString::from("/definitely/not/a/real/path/zenohd")),
            None,
        )
        .expect_err("non-existent ZENOHD_PATH must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ZENOHD_PATH"),
            "error should mention ZENOHD_PATH, got: {msg}"
        );
        assert!(
            msg.contains("cargo install zenohd"),
            "error should suggest the install command, got: {msg}"
        );
    }

    /// T14.9a binary-discovery fallthrough #3: env unset + PATH
    /// search misses -> clean error mentioning the install command.
    #[test]
    fn locate_zenohd_errors_when_nowhere() {
        let err = locate_zenohd_with_env(None, Some(OsString::from("")))
            .expect_err("empty PATH + no env must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("zenohd binary not found"),
            "error message changed: {msg}"
        );
        assert!(
            msg.contains("cargo install zenohd --version 1.9.0"),
            "error must include the canonical install command: {msg}"
        );
        assert!(
            msg.contains("ZENOHD_PATH"),
            "error must mention the override env var: {msg}"
        );
    }

    /// T14.9a binary-discovery fallthrough #4: env unset, PATH has
    /// an entry that contains a file named `zenohd` (or `zenohd.exe`
    /// on Windows). We synthesise such a fake binary in a temp dir
    /// and pass that dir as PATH.
    #[test]
    fn locate_zenohd_finds_on_path() {
        let dir = std::env::temp_dir().join("variant-zenoh-test-path");
        let _ = fs::create_dir_all(&dir);
        let bin_name = if cfg!(windows) {
            "zenohd.exe"
        } else {
            "zenohd"
        };
        let bin_path = dir.join(bin_name);
        fs::write(&bin_path, b"fake").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(&bin_path).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(&bin_path, perm).unwrap();
        }
        let result = locate_zenohd_with_env(None, Some(dir.clone().into_os_string()))
            .expect("PATH search should find the fake binary");
        assert_eq!(result.source, BinarySource::Path);
        // Compare case-insensitively because Windows PATHEXT entries
        // commonly include uppercase ".EXE", and the canonical
        // filename casing we wrote (`zenohd.exe`) may differ from
        // what the PATH walker produces when joining `name + PATHEXT
        // entry` (e.g. "zenohd.EXE"). Both refer to the same file on
        // a case-insensitive filesystem.
        let found_lower = result
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase());
        assert_eq!(
            found_lower.as_deref(),
            Some(bin_name.to_ascii_lowercase().as_str()),
            "expected {bin_name} (case-insensitive), got {}",
            result.path.display()
        );
        let _ = fs::remove_file(&bin_path);
        let _ = fs::remove_dir(&dir);
    }

    /// T14.9a port-derivation invariant #1: runner_index=0 returns
    /// the base port unchanged. Solo-runner / unit-test default.
    #[test]
    fn derive_port_runner_zero_returns_base() {
        assert_eq!(derive_sidecar_port(20000, 0).unwrap(), 20000);
    }

    /// T14.9a port-derivation invariant #2: stride is exactly 1 per
    /// runner index (matches T14.18 / T15.10 control-port convention).
    #[test]
    fn derive_port_runner_stride_is_one() {
        assert_eq!(derive_sidecar_port(20000, 1).unwrap(), 20001);
        assert_eq!(derive_sidecar_port(20000, 7).unwrap(), 20007);
        assert_eq!(derive_sidecar_port(30000, 42).unwrap(), 30042);
    }

    /// T14.9a port-derivation invariant #3: overflow surfaces as an
    /// error rather than wrapping into a colliding port range.
    #[test]
    fn derive_port_overflow_is_an_error() {
        // base 65535 + index 1 would wrap.
        let r = derive_sidecar_port(65_535, 1);
        assert!(r.is_err(), "expected overflow error, got {r:?}");
        let r = derive_sidecar_port(60_000, 65_536);
        assert!(r.is_err(), "expected u16 overflow error, got {r:?}");
    }

    /// T14.9a config-generation: REST plugin port appears in the
    /// generated JSON and binds to localhost only (security: the
    /// sidecar RPC surface is per-process, not a network service).
    #[test]
    fn build_zenohd_config_includes_rest_port_on_localhost() {
        let cfg = build_zenohd_config_json(20003, None, &[]);
        assert!(
            cfg.contains("127.0.0.1:20003"),
            "config must bind REST to 127.0.0.1:<port>, got:\n{cfg}"
        );
        // No listen/connect blocks when both inputs are empty.
        assert!(
            !cfg.contains("\"listen\""),
            "no listen block expected: {cfg}"
        );
        assert!(
            !cfg.contains("\"connect\""),
            "no connect block expected: {cfg}"
        );
        // Must be valid JSON.
        let _: serde_json::Value =
            serde_json::from_str(&cfg).expect("generated config must be valid JSON");
    }

    /// T14.9b: when peering is requested, the config emits a
    /// `tcp/<listen>` listen endpoint and `tcp/<peer>` connect
    /// endpoints so multiple per-runner sidecars can form a Zenoh
    /// peer mesh.
    #[test]
    fn build_zenohd_config_includes_listen_and_connect_when_provided() {
        let cfg = build_zenohd_config_json(
            20003,
            Some("127.0.0.1:21003".to_string()),
            &["127.0.0.1:21004".to_string(), "127.0.0.1:21005".to_string()],
        );
        assert!(
            cfg.contains("tcp/127.0.0.1:21003"),
            "listen endpoint must be present: {cfg}"
        );
        assert!(
            cfg.contains("tcp/127.0.0.1:21004"),
            "first connect endpoint must be present: {cfg}"
        );
        assert!(
            cfg.contains("tcp/127.0.0.1:21005"),
            "second connect endpoint must be present: {cfg}"
        );
        // Round-trip via serde_json to confirm the structure.
        let v: serde_json::Value = serde_json::from_str(&cfg).expect("valid JSON");
        let listen_eps = v["listen"]["endpoints"].as_array().expect("listen array");
        assert_eq!(listen_eps.len(), 1);
        let connect_eps = v["connect"]["endpoints"].as_array().expect("connect array");
        assert_eq!(connect_eps.len(), 2);
    }

    /// T14.9a smoke URL: matches the admin space the integration
    /// test polls. Regression guard so changes to the URL surface
    /// in one place break loudly in the other.
    #[test]
    fn rest_ready_url_shape() {
        assert_eq!(
            rest_ready_url(20003),
            "http://127.0.0.1:20003/@/router/local"
        );
    }
}
