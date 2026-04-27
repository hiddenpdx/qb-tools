use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "qb-move-after-days")]
#[command(
    about = "Move completed qBittorrent torrents to slower storage after a configured number of days"
)]
struct Args {
    #[arg(long, value_name = "FILE")]
    config: PathBuf,

    #[arg(long)]
    dry_run: bool,

    #[arg(long, value_name = "FOLDER")]
    log: Option<PathBuf>,

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
        qb_move_after_days::run_move_after_days_daemon(
            &args.config,
            args.dry_run,
            args.log.as_deref(),
            args.interval
                .unwrap_or(qb_move_after_days::DEFAULT_DAEMON_INTERVAL_SECS),
        )
    } else {
        qb_move_after_days::run_move_after_days(&args.config, args.dry_run, args.log.as_deref())
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
    fn daemon_accepts_custom_interval() {
        let args = Args::parse_from([
            "qb-move-after-days",
            "--config",
            "config.toml",
            "--daemon",
            "--interval",
            "42",
        ]);

        assert!(args.daemon);
        assert_eq!(args.interval, Some(42));
    }

    #[test]
    fn interval_requires_daemon() {
        let error = Args::try_parse_from([
            "qb-move-after-days",
            "--config",
            "config.toml",
            "--interval",
            "42",
        ])
        .expect_err("interval without daemon should fail");

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }
}
