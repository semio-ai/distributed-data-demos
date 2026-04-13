mod aeron;

use anyhow::{Context, Result};
use clap::Parser;

use variant_base::cli::CliArgs;
use variant_base::driver::run_protocol;

use crate::aeron::{AeronConfig, AeronVariant};

/// Parse variant-specific extra args from the CLI trailing arguments.
///
/// Expected extra args: `--aeron-dir <path>`, `--channel <uri>`, `--stream-id <id>`.
/// All are optional with defaults.
fn parse_extra_args(extra: &[String]) -> Result<AeronConfig> {
    let mut aeron_dir: Option<String> = None;
    let mut channel = "aeron:udp?endpoint=239.0.0.1:40456".to_string();
    let mut stream_id: i32 = 1001;

    let mut i = 0;
    while i < extra.len() {
        match extra[i].as_str() {
            "--aeron-dir" => {
                i += 1;
                aeron_dir = Some(
                    extra
                        .get(i)
                        .context("--aeron-dir requires a value")?
                        .clone(),
                );
            }
            "--channel" => {
                i += 1;
                channel = extra.get(i).context("--channel requires a value")?.clone();
            }
            "--stream-id" => {
                i += 1;
                stream_id = extra
                    .get(i)
                    .context("--stream-id requires a value")?
                    .parse()
                    .context("--stream-id must be an integer")?;
            }
            other => {
                return Err(anyhow::anyhow!("unknown extra argument: {}", other));
            }
        }
        i += 1;
    }

    Ok(AeronConfig {
        aeron_dir,
        channel,
        stream_id,
        runner: String::new(), // filled in by caller
    })
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {:#}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = CliArgs::parse();

    let mut config = parse_extra_args(&args.extra)?;
    config.runner = args.runner.clone();

    let mut variant = AeronVariant::new(config);
    run_protocol(&mut variant, &args)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_extra_args_defaults() {
        let config = parse_extra_args(&[]).unwrap();
        assert!(config.aeron_dir.is_none());
        assert_eq!(config.channel, "aeron:udp?endpoint=239.0.0.1:40456");
        assert_eq!(config.stream_id, 1001);
    }

    #[test]
    fn test_parse_extra_args_all_specified() {
        let extra = vec![
            "--aeron-dir".to_string(),
            "/tmp/aeron".to_string(),
            "--channel".to_string(),
            "aeron:ipc".to_string(),
            "--stream-id".to_string(),
            "2002".to_string(),
        ];
        let config = parse_extra_args(&extra).unwrap();
        assert_eq!(config.aeron_dir.as_deref(), Some("/tmp/aeron"));
        assert_eq!(config.channel, "aeron:ipc");
        assert_eq!(config.stream_id, 2002);
    }

    #[test]
    fn test_parse_extra_args_unknown_arg() {
        let extra = vec!["--unknown".to_string()];
        let result = parse_extra_args(&extra);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown extra argument"));
    }

    #[test]
    fn test_parse_extra_args_missing_value() {
        let extra = vec!["--channel".to_string()];
        let result = parse_extra_args(&extra);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_extra_args_invalid_stream_id() {
        let extra = vec!["--stream-id".to_string(), "not_a_number".to_string()];
        let result = parse_extra_args(&extra);
        assert!(result.is_err());
    }
}
