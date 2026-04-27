use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "qb-autoremove")]
#[command(about = "Remove qBittorrent torrents using autoremove-style task strategies")]
struct Args {
    #[arg(
        short = 'c',
        long = "config",
        alias = "conf",
        value_name = "FILE",
        default_value = "config.toml"
    )]
    config: PathBuf,

    #[arg(short = 'v', long = "dry-run", alias = "view")]
    dry_run: bool,

    #[arg(short = 't', long = "task", value_name = "NAME")]
    task: Option<String>,

    #[arg(short = 'l', long = "log", value_name = "FOLDER")]
    log: Option<PathBuf>,

    #[arg(short = 'd', long = "debug")]
    debug: bool,

    #[arg(long = "daemon")]
    daemon: bool,

    #[arg(
        long = "interval",
        value_name = "SECONDS",
        requires = "daemon",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    interval: Option<u64>,
}

fn main() {
    let args = Args::parse();
    let result = if args.daemon {
        qb_move_after_days::run_autoremove_daemon(
            &args.config,
            args.dry_run,
            args.log.as_deref(),
            args.task.as_deref(),
            args.debug,
            args.interval
                .unwrap_or(qb_move_after_days::DEFAULT_AUTOREMOVE_INTERVAL_SECS),
        )
    } else {
        qb_move_after_days::run_autoremove(
            &args.config,
            args.dry_run,
            args.log.as_deref(),
            args.task.as_deref(),
            args.debug,
        )
    };

    if let Err(error) = result {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn config_defaults_to_config_toml() {
        let args = Args::parse_from(["qb-autoremove"]);
        assert_eq!(args.config, PathBuf::from("config.toml"));
        assert!(!args.dry_run);
        assert!(args.task.is_none());
        assert!(args.log.is_none());
        assert!(!args.debug);
        assert!(!args.daemon);
        assert!(args.interval.is_none());
    }

    #[test]
    fn daemon_accepts_custom_interval() {
        let args = Args::parse_from(["qb-autoremove", "--daemon", "--interval", "42"]);
        assert!(args.daemon);
        assert_eq!(args.interval, Some(42));
    }

    #[test]
    fn interval_requires_daemon() {
        let error = Args::try_parse_from(["qb-autoremove", "--interval", "42"])
            .expect_err("interval without daemon should fail");
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }
}
