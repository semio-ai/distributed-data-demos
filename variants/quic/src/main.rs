mod certs;
mod quic;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;

use crate::quic::QuicVariant;

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();

    // Map name to an index based on first letter of name in alphabetical order
    let runner_idx = args.runner
        .to_lowercase()
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| (c as u8 - b'a') as usize)
        .context("Runner name must start with an alphabetic character (a-z)")?;

    let bind_addr_raw = parse_extra_arg(&args.extra, "bind-addr")
        .unwrap_or_else(|| "0.0.0.0:0".to_string());

    let bind_addr = bind_addr_raw
        .split(',')
        .nth(runner_idx) // Take the address at our index
        .context(format!("No bind-addr found for index {}", runner_idx))?
        .trim()
        .parse::<SocketAddr>()
        .context("invalid --bind-addr")?;

    let peers: Vec<SocketAddr> = parse_extra_arg(&args.extra, "peers")
        .unwrap_or_default()
        .split(',')
        .enumerate()
        .filter(|(i, _)| *i != runner_idx) // Skip the address that belongs to me
        .map(|(_, s)| s.trim().parse::<SocketAddr>())
        .collect::<Result<Vec<_>, _>>()
        .context("invalid --peers address")?;

    let mut variant = QuicVariant::new(&args.runner, bind_addr, peers);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extra_arg_found() {
        let extra: Vec<String> = vec![
            "--bind-addr".into(),
            "127.0.0.1:5000".into(),
            "--peers".into(),
            "127.0.0.1:5001".into(),
        ];
        assert_eq!(
            parse_extra_arg(&extra, "bind-addr"),
            Some("127.0.0.1:5000".to_string())
        );
        assert_eq!(
            parse_extra_arg(&extra, "peers"),
            Some("127.0.0.1:5001".to_string())
        );
    }

    #[test]
    fn test_parse_extra_arg_not_found() {
        let extra: Vec<String> = vec!["--bind-addr".into(), "127.0.0.1:5000".into()];
        assert_eq!(parse_extra_arg(&extra, "peers"), None);
    }

    #[test]
    fn test_parse_extra_arg_empty() {
        let extra: Vec<String> = vec![];
        assert_eq!(parse_extra_arg(&extra, "bind-addr"), None);
    }
}
