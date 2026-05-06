mod pairing;
mod protocol;
mod websocket;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;
use variant_base::types::Qos;

use crate::pairing::{derive_endpoints, parse_peers};
use crate::websocket::{WebSocketConfig, WebSocketVariant};

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();

    // Reject QoS 1 and 2 cleanly *before* any I/O. The variant only
    // implements reliable QoS (3 and 4) -- QoS 1-2 belong to Hybrid.
    if !(3..=4).contains(&args.qos) {
        bail!(
            "websocket variant only supports reliable QoS (3 or 4); got --qos {}",
            args.qos
        );
    }

    let qos = Qos::from_int(args.qos)
        .ok_or_else(|| anyhow!("invalid --qos {}; expected 1..=4", args.qos))?;

    let ws_base_port = parse_required_extra_arg(&args.extra, "ws-base-port")
        .context("missing required --ws-base-port in variant-specific args")?
        .parse::<u16>()
        .context("invalid --ws-base-port (expected u16)")?;

    let peers_raw = parse_required_extra_arg(&args.extra, "peers")
        .context("missing runner-injected --peers argument")?;
    let peer_map = parse_peers(&peers_raw).context("failed to parse --peers")?;

    let derived = derive_endpoints(&peer_map, &args.runner, ws_base_port, args.qos)
        .context("WebSocket port derivation failed")?;

    let config = WebSocketConfig::from_derived(derived, qos);
    let mut variant = WebSocketVariant::new(&args.runner, config);
    run_protocol(&mut variant, &args)?;
    Ok(())
}

/// Parse a `--key value` pair from the extra CLI arguments.
fn parse_extra_arg(extra: &[String], key: &str) -> Option<String> {
    let flag = format!("--{key}");
    let mut iter = extra.iter();
    while let Some(arg) = iter.next() {
        if arg == &flag {
            return iter.next().cloned();
        }
    }
    None
}

/// Parse a required `--key value` pair from the extra CLI arguments.
fn parse_required_extra_arg(extra: &[String], key: &str) -> Result<String> {
    parse_extra_arg(extra, key).ok_or_else(|| anyhow!("missing required --{key} argument"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extra_arg_found() {
        let extra: Vec<String> = vec![
            "--ws-base-port".into(),
            "19960".into(),
            "--peers".into(),
            "a=127.0.0.1".into(),
        ];
        assert_eq!(
            parse_extra_arg(&extra, "ws-base-port"),
            Some("19960".into())
        );
        assert_eq!(parse_extra_arg(&extra, "peers"), Some("a=127.0.0.1".into()));
    }

    #[test]
    fn parse_extra_arg_not_found() {
        let extra: Vec<String> = vec!["--ws-base-port".into(), "19960".into()];
        assert_eq!(parse_extra_arg(&extra, "peers"), None);
    }

    #[test]
    fn parse_extra_arg_empty() {
        let extra: Vec<String> = vec![];
        assert_eq!(parse_extra_arg(&extra, "ws-base-port"), None);
    }
}
