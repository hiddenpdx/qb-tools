use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use nix::sys::statvfs::statvfs;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{ORIGIN, REFERER};
use serde::Deserialize;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod autoremove;

pub use autoremove::DEFAULT_AUTOREMOVE_INTERVAL_SECS;
pub use autoremove::run_autoremove;
pub use autoremove::run_autoremove_daemon;
pub const DEFAULT_DAEMON_INTERVAL_SECS: u64 = DEFAULT_AUTOREMOVE_INTERVAL_SECS;

const SECONDS_PER_DAY: u64 = 86_400;
const MOVE_POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct Config {
    client: ClientConfig,
    rules: Vec<RuleConfig>,
}

#[derive(Debug, Deserialize)]
struct ClientConfig {
    host: String,
    username: String,
    password: String,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LoggingMode {
    #[default]
    Rotating,
    Single,
}

#[derive(Debug, Deserialize, Default)]
struct LoggingConfig {
    #[serde(default)]
    mode: LoggingMode,
}

#[derive(Debug, Deserialize, Default)]
struct LoggingBootstrap {
    #[serde(default)]
    logging: LoggingConfig,
}

#[derive(Debug, Deserialize)]
struct RuleConfig {
    source_path: PathBuf,
    target_path: PathBuf,
    min_days_since_completion: u64,
    min_free_space_percent: Option<f64>,
}

#[derive(Debug, Clone)]
struct Rule {
    index: usize,
    source_path: PathBuf,
    target_path: PathBuf,
    min_days_since_completion: u64,
    min_free_space_percent: Option<f64>,
    specificity: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct TorrentInfo {
    hash: String,
    name: String,
    progress: f64,
    completion_on: i64,
    save_path: String,
    size: u64,
    #[serde(default)]
    auto_tmm: bool,
    #[serde(default)]
    state: String,
}

#[derive(Debug, Default)]
struct AgeSummary {
    checked: usize,
    eligible: usize,
    dry_run_matches: usize,
    moved: usize,
    skipped: usize,
    failed: usize,
}

#[derive(Debug, Default)]
struct LowSpaceSummary {
    rules_checked: usize,
    rules_triggered: usize,
    batches_planned: usize,
    torrents_planned: usize,
    torrents_queued: usize,
    failed: usize,
}

#[derive(Debug, Clone)]
struct MoveCandidate {
    hash: String,
    name: String,
    save_path: PathBuf,
    destination: PathBuf,
    completion_on: u64,
    size: u64,
    auto_tmm: bool,
}

#[derive(Debug, Clone)]
struct QueuedMove {
    hash: String,
    name: String,
    destination: PathBuf,
}

#[derive(Debug, Clone, Copy)]
struct FilesystemUsage {
    total_bytes: u64,
    available_bytes: u64,
}

struct QbitClient {
    http: Client,
    base_url: Url,
    origin: String,
    referer: String,
}

pub fn run_move_after_days(
    config_path: &Path,
    dry_run: bool,
    log_dir: Option<&Path>,
) -> Result<()> {
    let logging_mode = load_logging_mode(config_path)?;
    let _log_guard = setup_logging(log_dir, "qb-move-after-days.log", logging_mode)?;

    let config = load_config(config_path)?;
    let rules = compile_rules(config.rules)?;
    log_loaded_rules(config_path, dry_run, &rules);

    run_move_after_days_once(&config.client, &rules, dry_run)
}

pub fn run_move_after_days_daemon(
    config_path: &Path,
    dry_run: bool,
    log_dir: Option<&Path>,
    interval_secs: u64,
) -> Result<()> {
    ensure!(
        interval_secs > 0,
        "daemon interval must be greater than zero seconds"
    );

    let logging_mode = load_logging_mode(config_path)?;
    let _log_guard = setup_logging(log_dir, "qb-move-after-days.log", logging_mode)?;

    let config = load_config(config_path)?;
    let rules = compile_rules(config.rules)?;
    log_loaded_rules(config_path, dry_run, &rules);
    info!(interval_secs, "starting move-after-days daemon");

    let interval = Duration::from_secs(interval_secs);
    let mut cycle = 0u64;
    loop {
        cycle += 1;
        info!(cycle, "starting move-after-days daemon cycle");

        if let Err(error) = run_move_after_days_once(&config.client, &rules, dry_run) {
            error!(cycle, error = %error, "move-after-days daemon cycle failed");
        }

        info!(
            cycle,
            interval_secs, "sleeping before next move-after-days daemon cycle"
        );
        sleep(interval);
    }
}

fn run_move_after_days_once(
    client_config: &ClientConfig,
    rules: &[Rule],
    dry_run: bool,
) -> Result<()> {
    let client = QbitClient::login(client_config)?;

    let now_epoch = current_unix_timestamp()?;
    let mut summary = AgeSummary::default();

    for torrent in client.list_torrents()? {
        summary.checked += 1;

        if torrent.progress < 1.0 {
            summary.skipped += 1;
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                progress = torrent.progress,
                "skipping incomplete torrent"
            );
            continue;
        }

        if torrent.completion_on <= 0 {
            summary.skipped += 1;
            warn!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                completion_on = torrent.completion_on,
                "skipping torrent without a valid completion timestamp"
            );
            continue;
        }

        if torrent.state == "moving" {
            summary.skipped += 1;
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                "skipping torrent that is already moving"
            );
            continue;
        }

        let save_path = normalize_path(Path::new(&torrent.save_path));
        let Some(rule) = match_rule(&rules, &save_path) else {
            summary.skipped += 1;
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                save_path = %save_path.display(),
                "skipping torrent because no rule matched its save path"
            );
            continue;
        };

        let age_seconds = now_epoch.saturating_sub(torrent.completion_on as u64);
        let min_age_seconds = rule
            .min_days_since_completion
            .saturating_mul(SECONDS_PER_DAY);
        if age_seconds < min_age_seconds {
            summary.skipped += 1;
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                age_days = age_seconds / SECONDS_PER_DAY,
                min_days_since_completion = rule.min_days_since_completion,
                "skipping torrent because it is not old enough yet"
            );
            continue;
        }

        let destination = remap_save_path(&save_path, rule)?;
        if destination == save_path {
            summary.skipped += 1;
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                save_path = %save_path.display(),
                target = %destination.display(),
                "skipping torrent because it is already in the destination path"
            );
            continue;
        }

        summary.eligible += 1;

        if dry_run {
            summary.dry_run_matches += 1;
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                source = %save_path.display(),
                target = %destination.display(),
                rule_source = %rule.source_path.display(),
                rule_target = %rule.target_path.display(),
                "dry-run: would move torrent"
            );
            continue;
        }

        if torrent.auto_tmm {
            info!(
                torrent = %torrent.name,
                hash = %torrent.hash,
                "disabling automatic torrent management before move"
            );
            if let Err(error) = client.set_auto_management(&torrent.hash, false) {
                summary.failed += 1;
                error!(
                    torrent = %torrent.name,
                    hash = %torrent.hash,
                    error = %error,
                    "failed to disable automatic torrent management"
                );
                continue;
            }
        }

        info!(
            torrent = %torrent.name,
            hash = %torrent.hash,
            source = %save_path.display(),
            target = %destination.display(),
            "moving torrent"
        );

        match client.set_location(&torrent.hash, &destination) {
            Ok(()) => {
                summary.moved += 1;
                info!(
                    torrent = %torrent.name,
                    hash = %torrent.hash,
                    target = %destination.display(),
                    "move requested successfully"
                );
            }
            Err(error) => {
                summary.failed += 1;
                error!(
                    torrent = %torrent.name,
                    hash = %torrent.hash,
                    target = %destination.display(),
                    error = %error,
                    "failed to move torrent"
                );
            }
        }
    }

    info!(
        checked = summary.checked,
        eligible = summary.eligible,
        dry_run_matches = summary.dry_run_matches,
        moved = summary.moved,
        skipped = summary.skipped,
        failed = summary.failed,
        "run finished"
    );

    if summary.failed > 0 {
        bail!("encountered {} failed torrent operations", summary.failed);
    }

    Ok(())
}

pub fn run_move_on_low_space(
    config_path: &Path,
    dry_run: bool,
    log_dir: Option<&Path>,
) -> Result<()> {
    let logging_mode = load_logging_mode(config_path)?;
    let _log_guard = setup_logging(log_dir, "qb-move-on-low-space.log", logging_mode)?;

    let config = load_config(config_path)?;
    let rules = compile_rules(config.rules)?;
    log_loaded_rules(config_path, dry_run, &rules);

    ensure_low_space_rules(&rules)?;

    run_move_on_low_space_once(&config.client, &rules, dry_run)
}

pub fn run_move_on_low_space_daemon(
    config_path: &Path,
    dry_run: bool,
    log_dir: Option<&Path>,
    interval_secs: u64,
) -> Result<()> {
    ensure!(
        interval_secs > 0,
        "daemon interval must be greater than zero seconds"
    );

    let logging_mode = load_logging_mode(config_path)?;
    let _log_guard = setup_logging(log_dir, "qb-move-on-low-space.log", logging_mode)?;

    let config = load_config(config_path)?;
    let rules = compile_rules(config.rules)?;
    log_loaded_rules(config_path, dry_run, &rules);
    ensure_low_space_rules(&rules)?;
    info!(interval_secs, "starting move-on-low-space daemon");

    let interval = Duration::from_secs(interval_secs);
    let mut cycle = 0u64;
    loop {
        cycle += 1;
        info!(cycle, "starting move-on-low-space daemon cycle");

        if let Err(error) = run_move_on_low_space_once(&config.client, &rules, dry_run) {
            error!(cycle, error = %error, "move-on-low-space daemon cycle failed");
        }

        info!(
            cycle,
            interval_secs, "sleeping before next move-on-low-space daemon cycle"
        );
        sleep(interval);
    }
}

fn ensure_low_space_rules(rules: &[Rule]) -> Result<()> {
    ensure!(
        rules
            .iter()
            .any(|rule| rule.min_free_space_percent.is_some()),
        "config must define at least one [[rules]] entry with min_free_space_percent"
    );

    Ok(())
}

fn run_move_on_low_space_once(
    client_config: &ClientConfig,
    rules: &[Rule],
    dry_run: bool,
) -> Result<()> {
    let client = QbitClient::login(client_config)?;
    let mut summary = LowSpaceSummary::default();

    for rule in rules {
        let Some(min_free_space_percent) = rule.min_free_space_percent else {
            info!(
                source = %rule.source_path.display(),
                target = %rule.target_path.display(),
                "skipping rule for low-space mover because min_free_space_percent is not configured"
            );
            continue;
        };

        summary.rules_checked += 1;
        let triggered = process_low_space_rule(
            &client,
            &rules,
            rule,
            min_free_space_percent,
            dry_run,
            &mut summary,
        )?;
        if triggered {
            summary.rules_triggered += 1;
        }
    }

    info!(
        rules_checked = summary.rules_checked,
        rules_triggered = summary.rules_triggered,
        batches_planned = summary.batches_planned,
        torrents_planned = summary.torrents_planned,
        torrents_queued = summary.torrents_queued,
        failed = summary.failed,
        "low-space run finished"
    );

    if summary.failed > 0 {
        bail!("encountered {} failed low-space operations", summary.failed);
    }

    Ok(())
}

fn process_low_space_rule(
    client: &QbitClient,
    rules: &[Rule],
    rule: &Rule,
    min_free_space_percent: f64,
    dry_run: bool,
    summary: &mut LowSpaceSummary,
) -> Result<bool> {
    let mut triggered = false;

    loop {
        let usage = filesystem_usage(&rule.source_path)?;
        let free_percent = free_space_percent(usage);
        info!(
            source = %rule.source_path.display(),
            target = %rule.target_path.display(),
            min_free_space_percent,
            free_space_percent = free_percent,
            available_bytes = usage.available_bytes,
            total_bytes = usage.total_bytes,
            "checked free space for rule"
        );

        if free_percent >= min_free_space_percent {
            if triggered {
                info!(
                    source = %rule.source_path.display(),
                    free_space_percent = free_percent,
                    min_free_space_percent,
                    "free-space threshold reached for rule"
                );
            }
            return Ok(triggered);
        }

        triggered = true;
        let deficit_bytes = free_space_deficit_bytes(usage, min_free_space_percent);
        let torrents = client.list_torrents()?;
        let candidates = collect_low_space_candidates(&torrents, rules, rule)?;

        if candidates.is_empty() {
            warn!(
                source = %rule.source_path.display(),
                deficit_bytes,
                "rule is below the free-space threshold but no movable torrents matched"
            );
            return Ok(triggered);
        }

        let batch = select_low_space_batch(&candidates, deficit_bytes);
        if batch.is_empty() {
            warn!(
                source = %rule.source_path.display(),
                deficit_bytes,
                "rule is below the free-space threshold but the candidates have no reclaimable size"
            );
            return Ok(triggered);
        }

        let batch_bytes = batch.iter().map(|candidate| candidate.size).sum::<u64>();
        summary.batches_planned += 1;
        summary.torrents_planned += batch.len();

        info!(
            source = %rule.source_path.display(),
            deficit_bytes,
            batch_bytes,
            torrents = batch.len(),
            "selected low-space move batch"
        );

        if dry_run {
            for candidate in &batch {
                info!(
                    torrent = %candidate.name,
                    hash = %candidate.hash,
                    completion_on = candidate.completion_on,
                    size = candidate.size,
                    source = %candidate.save_path.display(),
                    target = %candidate.destination.display(),
                    "dry-run: would queue torrent to recover free space"
                );
            }

            return Ok(triggered);
        }

        let queued = queue_low_space_batch(client, &batch, summary);
        if queued.is_empty() {
            warn!(
                source = %rule.source_path.display(),
                "no torrents could be queued from the selected low-space batch"
            );
            return Ok(triggered);
        }

        wait_for_moves_to_finish(client, &queued)?;
    }
}

fn queue_low_space_batch(
    client: &QbitClient,
    batch: &[MoveCandidate],
    summary: &mut LowSpaceSummary,
) -> Vec<QueuedMove> {
    let mut queued = Vec::with_capacity(batch.len());

    for candidate in batch {
        if candidate.auto_tmm {
            info!(
                torrent = %candidate.name,
                hash = %candidate.hash,
                "disabling automatic torrent management before low-space move"
            );
            if let Err(error) = client.set_auto_management(&candidate.hash, false) {
                summary.failed += 1;
                error!(
                    torrent = %candidate.name,
                    hash = %candidate.hash,
                    error = %error,
                    "failed to disable automatic torrent management before low-space move"
                );
                continue;
            }
        }

        info!(
            torrent = %candidate.name,
            hash = %candidate.hash,
            size = candidate.size,
            source = %candidate.save_path.display(),
            target = %candidate.destination.display(),
            "queueing torrent move for free-space recovery"
        );

        match client.set_location(&candidate.hash, &candidate.destination) {
            Ok(()) => {
                summary.torrents_queued += 1;
                queued.push(QueuedMove {
                    hash: candidate.hash.clone(),
                    name: candidate.name.clone(),
                    destination: candidate.destination.clone(),
                });
            }
            Err(error) => {
                summary.failed += 1;
                error!(
                    torrent = %candidate.name,
                    hash = %candidate.hash,
                    target = %candidate.destination.display(),
                    error = %error,
                    "failed to queue torrent move for free-space recovery"
                );
            }
        }
    }

    queued
}

fn wait_for_moves_to_finish(client: &QbitClient, queued: &[QueuedMove]) -> Result<()> {
    let hashes = queued
        .iter()
        .map(|move_request| move_request.hash.clone())
        .collect::<Vec<_>>();

    loop {
        let torrents = client.list_torrents_by_hashes(&hashes)?;
        let by_hash = torrents
            .iter()
            .map(|torrent| (torrent.hash.as_str(), torrent))
            .collect::<HashMap<_, _>>();

        let mut pending = 0usize;
        for move_request in queued {
            let Some(torrent) = by_hash.get(move_request.hash.as_str()) else {
                warn!(
                    torrent = %move_request.name,
                    hash = %move_request.hash,
                    "torrent disappeared from qBittorrent while waiting for move completion"
                );
                continue;
            };

            let current_path = normalize_path(Path::new(&torrent.save_path));
            let move_finished =
                torrent.state != "moving" && current_path == move_request.destination;
            if !move_finished {
                pending += 1;
            }
        }

        if pending == 0 {
            info!(torrents = queued.len(), "queued moves finished");
            return Ok(());
        }

        info!(pending, "waiting for queued moves to finish");
        sleep(MOVE_POLL_INTERVAL);
    }
}

fn collect_low_space_candidates(
    torrents: &[TorrentInfo],
    rules: &[Rule],
    current_rule: &Rule,
) -> Result<Vec<MoveCandidate>> {
    let mut candidates = Vec::new();

    for torrent in torrents {
        if torrent.progress < 1.0 || torrent.completion_on <= 0 || torrent.state == "moving" {
            continue;
        }

        let save_path = normalize_path(Path::new(&torrent.save_path));
        let Some(matched_rule) = match_rule(rules, &save_path) else {
            continue;
        };

        if matched_rule.index != current_rule.index {
            continue;
        }

        let destination = remap_save_path(&save_path, current_rule)?;
        if destination == save_path {
            continue;
        }

        if torrent.size == 0 {
            continue;
        }

        candidates.push(MoveCandidate {
            hash: torrent.hash.clone(),
            name: torrent.name.clone(),
            save_path,
            destination,
            completion_on: torrent.completion_on as u64,
            size: torrent.size,
            auto_tmm: torrent.auto_tmm,
        });
    }

    candidates.sort_by_key(|candidate| candidate.completion_on);
    Ok(candidates)
}

fn select_low_space_batch(candidates: &[MoveCandidate], required_bytes: u64) -> Vec<MoveCandidate> {
    if required_bytes == 0 {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut reclaimed = 0u64;
    for candidate in candidates {
        selected.push(candidate.clone());
        reclaimed = reclaimed.saturating_add(candidate.size);
        if reclaimed >= required_bytes {
            break;
        }
    }

    selected
}

fn filesystem_usage(path: &Path) -> Result<FilesystemUsage> {
    let stats = statvfs(path).with_context(|| {
        format!(
            "failed to read filesystem statistics for {}",
            path.display()
        )
    })?;
    let block_size = stats.fragment_size() as u64;
    let total_blocks = stats.blocks() as u64;
    let available_blocks = stats.blocks_available() as u64;

    Ok(FilesystemUsage {
        total_bytes: total_blocks.saturating_mul(block_size),
        available_bytes: available_blocks.saturating_mul(block_size),
    })
}

fn free_space_percent(usage: FilesystemUsage) -> f64 {
    if usage.total_bytes == 0 {
        return 0.0;
    }

    (usage.available_bytes as f64 / usage.total_bytes as f64) * 100.0
}

fn free_space_deficit_bytes(usage: FilesystemUsage, min_free_space_percent: f64) -> u64 {
    let target_available_bytes =
        ((usage.total_bytes as f64) * (min_free_space_percent / 100.0)).ceil() as u64;
    target_available_bytes.saturating_sub(usage.available_bytes)
}

fn setup_logging(
    log_dir: Option<&Path>,
    log_file_name: &str,
    mode: LoggingMode,
) -> Result<Option<WorkerGuard>> {
    setup_logging_with_level(log_dir, log_file_name, false, mode)
}

fn setup_logging_with_level(
    log_dir: Option<&Path>,
    log_file_name: &str,
    debug: bool,
    mode: LoggingMode,
) -> Result<Option<WorkerGuard>> {
    let level = if debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(std::io::stdout)
        .with_filter(level);

    if let Some(log_dir) = log_dir {
        fs::create_dir_all(log_dir)
            .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;

        let file_appender = match mode {
            LoggingMode::Rotating => tracing_appender::rolling::daily(log_dir, log_file_name),
            LoggingMode::Single => tracing_appender::rolling::never(log_dir, log_file_name),
        };
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(false)
            .with_writer(non_blocking)
            .with_filter(level);

        tracing_subscriber::registry()
            .with(console_layer)
            .with(file_layer)
            .try_init()
            .context("failed to initialize logging")?;

        return Ok(Some(guard));
    }

    tracing_subscriber::registry()
        .with(console_layer)
        .try_init()
        .context("failed to initialize logging")?;

    Ok(None)
}

fn load_logging_mode(config_path: &Path) -> Result<LoggingMode> {
    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    load_logging_mode_from_str(&raw)
        .with_context(|| format!("failed to parse config file {}", config_path.display()))
}

fn load_logging_mode_from_str(raw: &str) -> Result<LoggingMode> {
    let bootstrap: LoggingBootstrap = toml::from_str(raw)?;
    Ok(bootstrap.logging.mode)
}

fn load_config(config_path: &Path) -> Result<Config> {
    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    let config: Config = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config file {}", config_path.display()))?;
    ensure!(
        !config.rules.is_empty(),
        "config must define at least one [[rules]] entry"
    );
    Ok(config)
}

fn compile_rules(raw_rules: Vec<RuleConfig>) -> Result<Vec<Rule>> {
    let mut seen_sources = HashSet::new();
    let mut rules = Vec::with_capacity(raw_rules.len());

    for (index, raw_rule) in raw_rules.into_iter().enumerate() {
        let source_path = normalize_path(&raw_rule.source_path);
        let target_path = normalize_path(&raw_rule.target_path);

        ensure!(
            source_path.is_absolute(),
            "rule source_path must be absolute: {}",
            source_path.display()
        );
        ensure!(
            target_path.is_absolute(),
            "rule target_path must be absolute: {}",
            target_path.display()
        );
        ensure!(
            source_path != target_path,
            "rule source_path and target_path must differ: {}",
            source_path.display()
        );
        ensure!(
            seen_sources.insert(source_path.clone()),
            "duplicate rule source_path: {}",
            source_path.display()
        );

        if let Some(min_free_space_percent) = raw_rule.min_free_space_percent {
            ensure!(
                min_free_space_percent.is_finite()
                    && min_free_space_percent > 0.0
                    && min_free_space_percent < 100.0,
                "rule min_free_space_percent must be between 0 and 100: {}",
                min_free_space_percent
            );
        }

        let specificity = source_path.components().count();
        rules.push(Rule {
            index,
            source_path,
            target_path,
            min_days_since_completion: raw_rule.min_days_since_completion,
            min_free_space_percent: raw_rule.min_free_space_percent,
            specificity,
        });
    }

    Ok(rules)
}

fn log_loaded_rules(config_path: &Path, dry_run: bool, rules: &[Rule]) {
    info!(
        config = %config_path.display(),
        dry_run,
        rules = rules.len(),
        "loaded configuration"
    );

    for rule in rules {
        info!(
            source = %rule.source_path.display(),
            target = %rule.target_path.display(),
            min_days_since_completion = rule.min_days_since_completion,
            min_free_space_percent = rule.min_free_space_percent,
            "loaded rule"
        );
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        normalized
    }
}

fn match_rule<'a>(rules: &'a [Rule], save_path: &Path) -> Option<&'a Rule> {
    rules
        .iter()
        .filter(|rule| save_path.starts_with(&rule.source_path))
        .max_by_key(|rule| rule.specificity)
}

fn remap_save_path(save_path: &Path, rule: &Rule) -> Result<PathBuf> {
    let relative = save_path.strip_prefix(&rule.source_path).with_context(|| {
        format!(
            "save path {} does not start with rule source {}",
            save_path.display(),
            rule.source_path.display()
        )
    })?;

    let destination = if relative.as_os_str().is_empty() {
        rule.target_path.clone()
    } else {
        rule.target_path.join(relative)
    };

    Ok(normalize_path(&destination))
}

fn current_unix_timestamp() -> Result<u64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before unix epoch")?;
    Ok(now.as_secs())
}

impl QbitClient {
    fn login(config: &ClientConfig) -> Result<Self> {
        let base_url = normalize_base_url(&config.host)?;
        let origin = build_origin(&base_url)?;
        let referer = format!("{origin}/");
        let http = Client::builder()
            .cookie_store(true)
            .build()
            .context("failed to build HTTP client")?;

        let login_url = api_url(&base_url, "auth/login")?;
        let response = http
            .post(login_url)
            .header(REFERER, &referer)
            .header(ORIGIN, &origin)
            .form(&[
                ("username", config.username.as_str()),
                ("password", config.password.as_str()),
            ])
            .send()
            .context("failed to send qBittorrent login request")?;

        let status = response.status();
        let body = response
            .text()
            .context("failed to read qBittorrent login response")?;

        if !status.is_success() {
            bail!(
                "qBittorrent login failed with status {}: {}",
                status,
                body.trim()
            );
        }

        if body.trim() != "Ok." {
            bail!("qBittorrent login failed: {}", body.trim());
        }

        Ok(Self {
            http,
            base_url,
            origin,
            referer,
        })
    }

    fn list_torrents(&self) -> Result<Vec<TorrentInfo>> {
        self.list_torrents_internal(None)
    }

    fn list_torrents_by_hashes(&self, hashes: &[String]) -> Result<Vec<TorrentInfo>> {
        self.list_torrents_internal(Some(hashes))
    }

    fn list_torrents_internal(&self, hashes: Option<&[String]>) -> Result<Vec<TorrentInfo>> {
        let url = api_url(&self.base_url, "torrents/info")?;
        let mut request = self.http.get(url).header(REFERER, &self.referer);

        let hash_string;
        if let Some(hashes) = hashes {
            hash_string = hashes.join("|");
            request = request.query(&[("hashes", hash_string.as_str())]);
        }

        let response = request.send().context("failed to fetch torrent list")?;
        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .context("failed to read torrent list error body")?;
            bail!(
                "failed to fetch torrent list with status {}: {}",
                status,
                body.trim()
            );
        }

        response
            .json::<Vec<TorrentInfo>>()
            .context("failed to decode torrent list JSON")
    }

    fn set_auto_management(&self, hash: &str, enable: bool) -> Result<()> {
        let enable_value = if enable { "true" } else { "false" };
        self.post_form(
            "torrents/setAutoManagement",
            &[("hashes", hash), ("enable", enable_value)],
        )
    }

    fn set_location(&self, hash: &str, location: &Path) -> Result<()> {
        let location_string = location.to_str().ok_or_else(|| {
            anyhow!(
                "destination path is not valid UTF-8: {}",
                location.display()
            )
        })?;
        self.post_form(
            "torrents/setLocation",
            &[("hashes", hash), ("location", location_string)],
        )
    }

    fn post_form(&self, endpoint: &str, form: &[(&str, &str)]) -> Result<()> {
        let url = api_url(&self.base_url, endpoint)?;
        let response = self
            .http
            .post(url)
            .header(REFERER, &self.referer)
            .header(ORIGIN, &self.origin)
            .form(form)
            .send()
            .with_context(|| format!("failed to call qBittorrent endpoint {endpoint}"))?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = response
            .text()
            .with_context(|| format!("failed to read qBittorrent error body for {endpoint}"))?;
        bail!(
            "qBittorrent endpoint {} failed with status {}: {}",
            endpoint,
            status,
            body.trim()
        );
    }
}

fn normalize_base_url(host: &str) -> Result<Url> {
    let mut url =
        Url::parse(host).with_context(|| format!("invalid qBittorrent host URL: {host}"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "qBittorrent host must use http or https: {host}"
    );
    ensure!(
        url.host_str().is_some(),
        "qBittorrent host is missing a hostname: {host}"
    );

    let path = url.path().trim_matches('/');
    ensure!(
        path.is_empty(),
        "qBittorrent host must not include a path: {host}"
    );

    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);

    Ok(url)
}

fn build_origin(base_url: &Url) -> Result<String> {
    let host = base_url
        .host_str()
        .ok_or_else(|| anyhow!("qBittorrent host is missing a hostname"))?;

    Ok(match base_url.port() {
        Some(port) => format!("{}://{}:{}", base_url.scheme(), host, port),
        None => format!("{}://{}", base_url.scheme(), host),
    })
}

fn api_url(base_url: &Url, endpoint: &str) -> Result<Url> {
    base_url
        .join(&format!("/api/v2/{endpoint}"))
        .with_context(|| format!("failed to build API URL for endpoint {endpoint}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(source: &str, target: &str, days: u64, percent: Option<f64>) -> Rule {
        Rule {
            index: 0,
            source_path: normalize_path(Path::new(source)),
            target_path: normalize_path(Path::new(target)),
            min_days_since_completion: days,
            min_free_space_percent: percent,
            specificity: Path::new(source).components().count(),
        }
    }

    fn candidate(name: &str, completion_on: u64, size: u64) -> MoveCandidate {
        MoveCandidate {
            hash: format!("hash-{name}"),
            name: name.to_string(),
            save_path: PathBuf::from("/Volumes/SSD"),
            destination: PathBuf::from("/Volumes/HDD"),
            completion_on,
            size,
            auto_tmm: false,
        }
    }

    #[test]
    fn chooses_most_specific_matching_rule() {
        let rules = vec![
            rule("/Volumes/SSD", "/Volumes/HDD", 14, None),
            rule("/Volumes/SSD/Movies", "/Volumes/HDD/Movies", 7, None),
        ];

        let matched = match_rule(&rules, Path::new("/Volumes/SSD/Movies/4K"))
            .expect("expected a matching rule");

        assert_eq!(matched.source_path, PathBuf::from("/Volumes/SSD/Movies"));
    }

    #[test]
    fn remaps_path_while_preserving_relative_structure() {
        let rule = rule("/Volumes/SSD", "/Volumes/HDD", 14, None);
        let destination = remap_save_path(Path::new("/Volumes/SSD/TV/Show"), &rule)
            .expect("expected a remapped path");

        assert_eq!(destination, PathBuf::from("/Volumes/HDD/TV/Show"));
    }

    #[test]
    fn remaps_root_rule_path_without_extra_segments() {
        let rule = rule("/Volumes/SSD/Movies", "/Volumes/HDD/Movies", 14, None);
        let destination = remap_save_path(Path::new("/Volumes/SSD/Movies"), &rule)
            .expect("expected a remapped path");

        assert_eq!(destination, PathBuf::from("/Volumes/HDD/Movies"));
    }

    #[test]
    fn normalize_path_removes_trailing_and_current_dir_segments() {
        let normalized = normalize_path(Path::new("/Volumes/SSD/Movies/./"));
        assert_eq!(normalized, PathBuf::from("/Volumes/SSD/Movies"));
    }

    #[test]
    fn does_not_match_paths_that_only_share_a_string_prefix() {
        let rules = vec![rule("/Volumes/SSD", "/Volumes/HDD", 14, None)];

        let matched = match_rule(&rules, Path::new("/Volumes/SSD2/Movies"));
        assert!(matched.is_none());
    }

    #[test]
    fn selects_oldest_candidates_until_deficit_is_covered() {
        let candidates = vec![
            candidate("oldest", 10, 100),
            candidate("older", 20, 150),
            candidate("newer", 30, 400),
        ];

        let selected = select_low_space_batch(&candidates, 220);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].name, "oldest");
        assert_eq!(selected[1].name, "older");
    }

    #[test]
    fn returns_all_candidates_if_deficit_exceeds_available_space_to_reclaim() {
        let candidates = vec![candidate("oldest", 10, 100), candidate("older", 20, 150)];

        let selected = select_low_space_batch(&candidates, 1_000);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn calculates_free_space_deficit_from_percentage() {
        let usage = FilesystemUsage {
            total_bytes: 1_000,
            available_bytes: 80,
        };

        assert_eq!(free_space_deficit_bytes(usage, 10.0), 20);
    }

    #[test]
    fn logging_mode_defaults_to_rotating_when_section_is_missing() {
        let mode = load_logging_mode_from_str(
            r#"
[client]
host = "http://127.0.0.1:8080"
username = "admin"
password = "secret"

[[rules]]
source_path = "/src"
target_path = "/dst"
min_days_since_completion = 7
"#,
        )
        .expect("logging mode parses");

        assert_eq!(mode, LoggingMode::Rotating);
    }

    #[test]
    fn logging_mode_parses_single() {
        let mode = load_logging_mode_from_str(
            r#"
[logging]
mode = "single"

[client]
host = "http://127.0.0.1:8080"
username = "admin"
password = "secret"

[[rules]]
source_path = "/src"
target_path = "/dst"
min_days_since_completion = 7
"#,
        )
        .expect("logging mode parses");

        assert_eq!(mode, LoggingMode::Single);
    }

    #[test]
    fn logging_mode_rejects_unknown_values() {
        let error = load_logging_mode_from_str(
            r#"
[logging]
mode = "weekly"
"#,
        )
        .expect_err("invalid logging mode should fail");

        assert!(error.to_string().contains("unknown variant"));
    }
}
