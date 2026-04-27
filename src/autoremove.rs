use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{ORIGIN, REFERER};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, TableLike};
use tracing::{debug, error, info, warn};

use super::{
    ClientConfig, api_url, build_origin, current_unix_timestamp, filesystem_usage,
    load_logging_mode, normalize_base_url, setup_logging_with_level,
};

const BYTES_PER_GIB: f64 = 1_073_741_824.0;
const BYTES_PER_KIB: f64 = 1024.0;
pub const DEFAULT_AUTOREMOVE_INTERVAL_SECS: u64 = 600;

pub fn run_autoremove(
    config_path: &Path,
    dry_run: bool,
    log_dir: Option<&Path>,
    selected_task: Option<&str>,
    debug_enabled: bool,
) -> Result<()> {
    let logging_mode = load_logging_mode(config_path)?;
    let _log_guard =
        setup_logging_with_level(log_dir, "qb-autoremove.log", debug_enabled, logging_mode)?;

    let config = load_config(config_path)?;
    info!(
        config = %config_path.display(),
        dry_run,
        tasks = config.tasks.len(),
        "loaded autoremove configuration"
    );

    run_autoremove_once(&config, dry_run, selected_task)
}

pub fn run_autoremove_daemon(
    config_path: &Path,
    dry_run: bool,
    log_dir: Option<&Path>,
    selected_task: Option<&str>,
    debug_enabled: bool,
    interval_secs: u64,
) -> Result<()> {
    ensure!(
        interval_secs > 0,
        "daemon interval must be greater than zero seconds"
    );

    let logging_mode = load_logging_mode(config_path)?;
    let _log_guard =
        setup_logging_with_level(log_dir, "qb-autoremove.log", debug_enabled, logging_mode)?;

    let config = load_config(config_path)?;
    validate_selected_task(&config, selected_task)?;
    info!(
        config = %config_path.display(),
        dry_run,
        tasks = config.tasks.len(),
        interval_secs,
        "loaded autoremove configuration"
    );
    info!(
        selected_task = selected_task.unwrap_or("all"),
        interval_secs, "starting autoremove daemon"
    );

    let interval = Duration::from_secs(interval_secs);
    let mut cycle = 0u64;
    loop {
        cycle += 1;
        info!(cycle, "starting autoremove daemon cycle");

        if let Err(error) = run_autoremove_once(&config, dry_run, selected_task) {
            error!(cycle, error = %error, "autoremove daemon cycle failed");
            debug!(cycle, error = ?error, "autoremove daemon cycle failure details");
        }

        info!(
            cycle,
            interval_secs, "sleeping before next autoremove daemon cycle"
        );
        sleep(interval);
    }
}

fn run_autoremove_once(
    config: &AutoremoveConfig,
    dry_run: bool,
    selected_task: Option<&str>,
) -> Result<()> {
    let client = QbitAutoremoveClient::login(&config.client)?;

    if let Some(task_name) = selected_task {
        let task = config
            .tasks
            .iter()
            .find(|task| task.name == task_name)
            .ok_or_else(|| anyhow!("no autoremove task named {task_name}"))?;
        run_task(&client, task, dry_run)?;
        return Ok(());
    }

    let failed_tasks = run_all_tasks(&config.tasks, |task| run_task(&client, task, dry_run));
    if failed_tasks > 0 {
        warn!(
            failed_tasks,
            total_tasks = config.tasks.len(),
            "completed autoremove run with task failures"
        );
    }

    Ok(())
}

fn validate_selected_task(config: &AutoremoveConfig, selected_task: Option<&str>) -> Result<()> {
    if let Some(task_name) = selected_task {
        ensure!(
            config.tasks.iter().any(|task| task.name == task_name),
            "no autoremove task named {task_name}"
        );
    }

    Ok(())
}

fn run_all_tasks<F>(tasks: &[AutoremoveTask], mut run_task_fn: F) -> usize
where
    F: FnMut(&AutoremoveTask) -> Result<()>,
{
    let mut failed_tasks = 0usize;
    for task in tasks {
        if let Err(error) = run_task_fn(task) {
            failed_tasks += 1;
            error!(task = %task.name, error = %error, "autoremove task failed");
            debug!(task = %task.name, error = ?error, "autoremove task failure details");
        }
    }
    failed_tasks
}

#[derive(Debug)]
struct AutoremoveConfig {
    client: ClientConfig,
    tasks: Vec<AutoremoveTask>,
}

#[derive(Debug)]
struct AutoremoveTask {
    name: String,
    delete_data: bool,
    strategies: Vec<StrategyConfig>,
}

#[derive(Debug)]
struct StrategyConfig {
    name: String,
    filters: FilterConfig,
    conditions: Vec<ConditionSpec>,
}

#[derive(Debug, Default)]
struct FilterConfig {
    all_categories: bool,
    categories: HashSet<String>,
    excluded_categories: HashSet<String>,
    all_trackers: bool,
    trackers: HashSet<String>,
    excluded_trackers: HashSet<String>,
    all_status: bool,
    status: StatusSelector,
    excluded_status: StatusSelector,
}

#[derive(Debug, Default)]
struct StatusSelector {
    statuses: HashSet<TorrentStatus>,
    stalled_upload: bool,
    stalled_download: bool,
}

#[derive(Debug, Clone)]
enum ConditionSpec {
    Nothing,
    Ratio(f64),
    CreateTime(f64),
    DownloadingTime(f64),
    SeedingTime(f64),
    MaxDownload(f64),
    MaxDownloadSpeed(f64),
    MinUploadSpeed(f64),
    MaxAverageDownloadSpeed(f64),
    MinAverageUploadSpeed(f64),
    MaxSize(f64),
    MaxSeeder(f64),
    MaxUpload(f64),
    MinLeecher(f64),
    MaxConnectedSeeder(f64),
    MinConnectedLeecher(f64),
    LastActivity(LastActivityValue),
    MaxProgress(f64),
    UploadRatio(f64),
    SeedSize {
        limit_gib: f64,
        action: SortAction,
    },
    MaximumNumber {
        limit: usize,
        action: SortAction,
    },
    FreeSpace {
        min_gib: f64,
        path: PathBuf,
        action: SortAction,
    },
    RemoteFreeSpace {
        min_gib: f64,
        path: String,
        action: SortAction,
    },
    Remove(Expression),
}

#[derive(Debug, Clone)]
enum LastActivityValue {
    Seconds(f64),
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortAction {
    RemoveOldSeeds,
    RemoveNewSeeds,
    RemoveBigSeeds,
    RemoveSmallSeeds,
    RemoveActiveSeeds,
    RemoveInactiveSeeds,
    RemoveFastUploadSeeds,
    RemoveSlowUploadSeeds,
}

#[derive(Debug, Clone, PartialEq)]
enum Expression {
    Relation {
        parameter: Parameter,
        comparer: Comparer,
        value: Literal,
    },
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parameter {
    AverageDownloadSpeed,
    AverageUploadSpeed,
    ConnectedLeecher,
    ConnectedSeeder,
    CreateTime,
    Download,
    DownloadSpeed,
    DownloadingTime,
    LastActivity,
    Leecher,
    Progress,
    Ratio,
    Seeder,
    SeedingTime,
    Size,
    Upload,
    UploadRatio,
    UploadSpeed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparer {
    Lt,
    Gt,
    Eq,
}

#[derive(Debug, Clone, PartialEq)]
enum Literal {
    Number(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TorrentStatus {
    Downloading,
    Uploading,
    Checking,
    Queued,
    Paused,
    Stopped,
    Error,
    Unknown,
}

#[derive(Debug, Clone)]
struct Torrent {
    hash: String,
    name: String,
    category: Vec<String>,
    tracker: Vec<String>,
    status: TorrentStatus,
    stalled: bool,
    size: u64,
    ratio: f64,
    uploaded: i64,
    downloaded: i64,
    create_time: i64,
    seeding_time: i64,
    downloading_time: i64,
    upload_speed: i64,
    average_upload_speed: i64,
    download_speed: i64,
    average_download_speed: i64,
    last_activity: Option<u64>,
    seeder: i64,
    connected_seeder: i64,
    leecher: i64,
    connected_leecher: i64,
    progress: f64,
}

#[derive(Debug, Default, Clone)]
struct ClientStatus {
    download_speed: u64,
    total_downloaded: u64,
    upload_speed: u64,
    total_uploaded: u64,
    free_space_on_disk: Option<u64>,
}

#[derive(Debug, Default)]
struct TaskSummary {
    torrents_seen: usize,
    candidates: usize,
    deleted: usize,
}

#[derive(Debug)]
struct DeleteCandidate {
    torrent: Torrent,
    strategy: String,
}

fn record_delete_candidate(
    remove_by_hash: &mut HashMap<String, DeleteCandidate>,
    torrent: Torrent,
    strategy_name: &str,
) {
    remove_by_hash
        .entry(torrent.hash.clone())
        .or_insert_with(|| DeleteCandidate {
            torrent,
            strategy: strategy_name.to_string(),
        });
}

fn run_task(client: &QbitAutoremoveClient, task: &AutoremoveTask, dry_run: bool) -> Result<()> {
    info!(task = %task.name, dry_run, strategies = task.strategies.len(), "running autoremove task");

    let client_status = client.client_status()?;
    info!(
        task = %task.name,
        download_speed = client_status.download_speed,
        total_downloaded = client_status.total_downloaded,
        upload_speed = client_status.upload_speed,
        total_uploaded = client_status.total_uploaded,
        free_space_on_disk = ?client_status.free_space_on_disk,
        "fetched qBittorrent client status"
    );
    let torrents = client.list_torrents()?;
    let mut remove_by_hash = HashMap::new();
    let now = current_unix_timestamp_i64()?;
    let mut summary = TaskSummary {
        torrents_seen: torrents.len(),
        ..TaskSummary::default()
    };

    for strategy in &task.strategies {
        let filtered = apply_filters(&strategy.filters, &torrents);
        info!(
            task = %task.name,
            strategy = %strategy.name,
            filtered = filtered.len(),
            conditions = strategy.conditions.len(),
            "running strategy"
        );

        let mut remain = filtered;
        let mut strategy_remove = Vec::new();

        for condition in &strategy.conditions {
            let (next_remain, newly_removed) =
                apply_condition(condition, &client_status, remain, now)?;
            remain = next_remain;
            strategy_remove.extend(newly_removed);
        }

        for torrent in strategy_remove {
            record_delete_candidate(&mut remove_by_hash, torrent, &strategy.name);
        }
    }

    summary.candidates = remove_by_hash.len();

    if dry_run {
        for candidate in remove_by_hash.values() {
            info!(
                task = %task.name,
                strategy = %candidate.strategy,
                torrent = %candidate.torrent.name,
                hash = %candidate.torrent.hash,
                delete_data = task.delete_data,
                "dry-run: would delete torrent"
            );
        }
    } else if !remove_by_hash.is_empty() {
        let hashes = remove_by_hash.keys().cloned().collect::<Vec<_>>();
        match client.delete_torrents(&hashes, task.delete_data) {
            Ok(()) => {
                summary.deleted = hashes.len();
                for candidate in remove_by_hash.values() {
                    info!(
                        task = %task.name,
                        strategy = %candidate.strategy,
                        torrent = %candidate.torrent.name,
                        hash = %candidate.torrent.hash,
                        delete_data = task.delete_data,
                        "deleted torrent"
                    );
                }
            }
            Err(error) => {
                for candidate in remove_by_hash.values() {
                    error!(
                        task = %task.name,
                        strategy = %candidate.strategy,
                        torrent = %candidate.torrent.name,
                        hash = %candidate.torrent.hash,
                        delete_data = task.delete_data,
                        error = %error,
                        "failed to delete torrent"
                    );
                }
                return Err(error);
            }
        }
    }

    info!(
        task = %task.name,
        torrents_seen = summary.torrents_seen,
        candidates = summary.candidates,
        deleted = summary.deleted,
        delete_data = task.delete_data,
        "autoremove task finished"
    );

    Ok(())
}

fn load_config(config_path: &Path) -> Result<AutoremoveConfig> {
    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    let document = raw
        .parse::<DocumentMut>()
        .with_context(|| format!("failed to parse config file {}", config_path.display()))?;

    let client = parse_client_config(&document)?;
    let tasks = parse_tasks(&document)?;
    ensure!(
        !tasks.is_empty(),
        "config must define at least one [[tasks]] entry for qb-autoremove"
    );

    Ok(AutoremoveConfig { client, tasks })
}

fn parse_client_config(document: &DocumentMut) -> Result<ClientConfig> {
    let client = document
        .get("client")
        .and_then(Item::as_table_like)
        .ok_or_else(|| anyhow!("config must define a [client] table"))?;

    Ok(ClientConfig {
        host: resolve_env(get_required_string(client, "host", "client.host")?),
        username: resolve_env(get_required_string(client, "username", "client.username")?),
        password: resolve_env(get_required_string(client, "password", "client.password")?),
    })
}

fn parse_tasks(document: &DocumentMut) -> Result<Vec<AutoremoveTask>> {
    let tasks_item = document
        .get("tasks")
        .ok_or_else(|| anyhow!("config must define at least one [[tasks]] entry"))?;
    let tasks_array = tasks_item
        .as_array_of_tables()
        .ok_or_else(|| anyhow!("tasks must be declared as [[tasks]]"))?;

    let mut tasks = Vec::with_capacity(tasks_array.len());
    let mut seen_names = HashSet::new();
    for task_table in tasks_array.iter() {
        let task = parse_task(task_table)?;
        ensure!(
            seen_names.insert(task.name.clone()),
            "duplicate task name: {}",
            task.name
        );
        tasks.push(task);
    }
    Ok(tasks)
}

fn parse_task(task_table: &Table) -> Result<AutoremoveTask> {
    let name = get_required_string(task_table, "name", "tasks.name")?;
    let delete_data = get_optional_bool(task_table, "delete_data")?.unwrap_or(false);

    let strategies_item = task_table.get("strategies").ok_or_else(|| {
        anyhow!("task {name} must define at least one [[tasks.strategies]] entry")
    })?;
    let strategies_array = strategies_item.as_array_of_tables().ok_or_else(|| {
        anyhow!("task {name} strategies must be declared as [[tasks.strategies]]")
    })?;
    ensure!(
        !strategies_array.is_empty(),
        "task {name} must define at least one [[tasks.strategies]] entry"
    );

    let mut strategies = Vec::with_capacity(strategies_array.len());
    let mut seen_names = HashSet::new();
    for strategy_table in strategies_array.iter() {
        let strategy = parse_strategy(strategy_table)?;
        ensure!(
            seen_names.insert(strategy.name.clone()),
            "task {name} has duplicate strategy name {}",
            strategy.name
        );
        strategies.push(strategy);
    }

    Ok(AutoremoveTask {
        name,
        delete_data,
        strategies,
    })
}

fn parse_strategy(strategy_table: &Table) -> Result<StrategyConfig> {
    let mut strategy_name = None;
    let mut filters = FilterConfig::default();
    let mut categories_seen = false;
    let mut trackers_seen = false;
    let mut status_seen = false;
    let mut all_categories_explicit = None;
    let mut all_trackers_explicit = None;
    let mut all_status_explicit = None;
    let mut conditions = Vec::new();

    for (key, item) in strategy_table.iter() {
        match key {
            "name" => strategy_name = Some(parse_string_item(item, "tasks.strategies.name")?),
            "all_categories" => {
                all_categories_explicit = Some(parse_bool_item(item, "all_categories")?)
            }
            "categories" => {
                categories_seen = true;
                filters.categories = parse_string_set(item, "categories")?;
            }
            "excluded_categories" => {
                filters.excluded_categories = parse_string_set(item, "excluded_categories")?;
            }
            "all_trackers" => all_trackers_explicit = Some(parse_bool_item(item, "all_trackers")?),
            "trackers" => {
                trackers_seen = true;
                filters.trackers = parse_string_set(item, "trackers")?;
            }
            "excluded_trackers" => {
                filters.excluded_trackers = parse_string_set(item, "excluded_trackers")?;
            }
            "all_status" => all_status_explicit = Some(parse_bool_item(item, "all_status")?),
            "status" => {
                status_seen = true;
                filters.status = parse_status_selector(item, "status")?;
            }
            "excluded_status" => {
                filters.excluded_status = parse_status_selector(item, "excluded_status")?;
            }
            "nothing" => conditions.push(ConditionSpec::Nothing),
            "ratio" => conditions.push(ConditionSpec::Ratio(parse_number_item(item, "ratio")?)),
            "create_time" => conditions.push(ConditionSpec::CreateTime(parse_number_item(
                item,
                "create_time",
            )?)),
            "downloading_time" => conditions.push(ConditionSpec::DownloadingTime(
                parse_number_item(item, "downloading_time")?,
            )),
            "seeding_time" => conditions.push(ConditionSpec::SeedingTime(parse_number_item(
                item,
                "seeding_time",
            )?)),
            "max_download" => conditions.push(ConditionSpec::MaxDownload(parse_number_item(
                item,
                "max_download",
            )?)),
            "max_downloadspeed" => conditions.push(ConditionSpec::MaxDownloadSpeed(
                parse_number_item(item, "max_downloadspeed")?,
            )),
            "min_uploadspeed" => conditions.push(ConditionSpec::MinUploadSpeed(parse_number_item(
                item,
                "min_uploadspeed",
            )?)),
            "max_average_downloadspeed" => conditions.push(ConditionSpec::MaxAverageDownloadSpeed(
                parse_number_item(item, "max_average_downloadspeed")?,
            )),
            "min_average_uploadspeed" => conditions.push(ConditionSpec::MinAverageUploadSpeed(
                parse_number_item(item, "min_average_uploadspeed")?,
            )),
            "max_size" => {
                conditions.push(ConditionSpec::MaxSize(parse_number_item(item, "max_size")?))
            }
            "max_seeder" => conditions.push(ConditionSpec::MaxSeeder(parse_number_item(
                item,
                "max_seeder",
            )?)),
            "max_upload" => conditions.push(ConditionSpec::MaxUpload(parse_number_item(
                item,
                "max_upload",
            )?)),
            "min_leecher" => conditions.push(ConditionSpec::MinLeecher(parse_number_item(
                item,
                "min_leecher",
            )?)),
            "max_connected_seeder" => conditions.push(ConditionSpec::MaxConnectedSeeder(
                parse_number_item(item, "max_connected_seeder")?,
            )),
            "min_connected_leecher" => conditions.push(ConditionSpec::MinConnectedLeecher(
                parse_number_item(item, "min_connected_leecher")?,
            )),
            "last_activity" => {
                conditions.push(ConditionSpec::LastActivity(parse_last_activity(item)?))
            }
            "max_progress" => conditions.push(ConditionSpec::MaxProgress(parse_number_item(
                item,
                "max_progress",
            )?)),
            "upload_ratio" => conditions.push(ConditionSpec::UploadRatio(parse_number_item(
                item,
                "upload_ratio",
            )?)),
            "seed_size" => conditions.push(parse_seed_size(item)?),
            "maximum_number" => conditions.push(parse_maximum_number(item)?),
            "free_space" => conditions.push(parse_free_space(item)?),
            "remote_free_space" => conditions.push(parse_remote_free_space(item)?),
            "remove" => conditions.push(ConditionSpec::Remove(parse_expression(
                parse_string_item(item, "remove")?.as_str(),
            )?)),
            _ => {}
        }
    }

    filters.all_categories = all_categories_explicit.unwrap_or(!categories_seen);
    filters.all_trackers = all_trackers_explicit.unwrap_or(!trackers_seen);
    filters.all_status = all_status_explicit.unwrap_or(!status_seen);

    Ok(StrategyConfig {
        name: strategy_name
            .ok_or_else(|| anyhow!("each [[tasks.strategies]] entry must define name"))?,
        filters,
        conditions,
    })
}

fn parse_seed_size(item: &Item) -> Result<ConditionSpec> {
    let table = item
        .as_table_like()
        .ok_or_else(|| anyhow!("seed_size must be an inline table with limit and action"))?;
    Ok(ConditionSpec::SeedSize {
        limit_gib: get_required_number(table, "limit", "seed_size.limit")?,
        action: parse_sort_action(
            get_required_string(table, "action", "seed_size.action")?.as_str(),
        )?,
    })
}

fn parse_maximum_number(item: &Item) -> Result<ConditionSpec> {
    let table = item
        .as_table_like()
        .ok_or_else(|| anyhow!("maximum_number must be an inline table with limit and action"))?;
    let limit = get_required_number(table, "limit", "maximum_number.limit")?;
    ensure!(limit >= 0.0, "maximum_number.limit must be non-negative");
    Ok(ConditionSpec::MaximumNumber {
        limit: limit as usize,
        action: parse_sort_action(
            get_required_string(table, "action", "maximum_number.action")?.as_str(),
        )?,
    })
}

fn parse_free_space(item: &Item) -> Result<ConditionSpec> {
    let table = item
        .as_table_like()
        .ok_or_else(|| anyhow!("free_space must be an inline table with min, path and action"))?;
    Ok(ConditionSpec::FreeSpace {
        min_gib: get_required_number(table, "min", "free_space.min")?,
        path: PathBuf::from(get_required_string(table, "path", "free_space.path")?),
        action: parse_sort_action(
            get_required_string(table, "action", "free_space.action")?.as_str(),
        )?,
    })
}

fn parse_remote_free_space(item: &Item) -> Result<ConditionSpec> {
    let table = item.as_table_like().ok_or_else(|| {
        anyhow!("remote_free_space must be an inline table with min, path and action")
    })?;
    Ok(ConditionSpec::RemoteFreeSpace {
        min_gib: get_required_number(table, "min", "remote_free_space.min")?,
        path: get_required_string(table, "path", "remote_free_space.path")?,
        action: parse_sort_action(
            get_required_string(table, "action", "remote_free_space.action")?.as_str(),
        )?,
    })
}

fn parse_last_activity(item: &Item) -> Result<LastActivityValue> {
    if let Some(text) = item.as_str() {
        let lower = text.to_ascii_lowercase();
        if lower == "never" || lower == "none" {
            return Ok(LastActivityValue::Never);
        }
        bail!("last_activity string value must be Never or None")
    }
    Ok(LastActivityValue::Seconds(parse_number_item(
        item,
        "last_activity",
    )?))
}

fn apply_filters(filters: &FilterConfig, torrents: &[Torrent]) -> Vec<Torrent> {
    torrents
        .iter()
        .filter(|torrent| category_matches(filters, torrent))
        .filter(|torrent| tracker_matches(filters, torrent))
        .filter(|torrent| status_matches(filters, torrent))
        .cloned()
        .collect()
}

fn category_matches(filters: &FilterConfig, torrent: &Torrent) -> bool {
    let mut accepted = filters.all_categories;
    if !accepted && !filters.categories.is_empty() {
        accepted = torrent
            .category
            .iter()
            .any(|category| filters.categories.contains(category));
    }
    if !accepted {
        return false;
    }
    !torrent
        .category
        .iter()
        .any(|category| filters.excluded_categories.contains(category))
}

fn tracker_matches(filters: &FilterConfig, torrent: &Torrent) -> bool {
    let mut accepted = filters.all_trackers;
    if !accepted && !filters.trackers.is_empty() {
        accepted = torrent
            .tracker
            .iter()
            .any(|tracker| tracker_selected(tracker, &filters.trackers));
    }
    if !accepted {
        return false;
    }
    !torrent
        .tracker
        .iter()
        .any(|tracker| tracker_selected(tracker, &filters.excluded_trackers))
}

fn tracker_selected(tracker: &str, selected: &HashSet<String>) -> bool {
    if selected.contains(tracker) {
        return true;
    }

    Url::parse(tracker)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|hostname| selected.contains(&hostname))
}

fn status_matches(filters: &FilterConfig, torrent: &Torrent) -> bool {
    let accepted = if filters.all_status {
        true
    } else {
        matches_status_selector(&filters.status, torrent)
    };

    if !accepted {
        return false;
    }

    !matches_status_selector(&filters.excluded_status, torrent)
}

fn matches_status_selector(selector: &StatusSelector, torrent: &Torrent) -> bool {
    selector.statuses.contains(&torrent.status)
        || (selector.stalled_upload
            && torrent.status == TorrentStatus::Uploading
            && torrent.stalled)
        || (selector.stalled_download
            && torrent.status == TorrentStatus::Downloading
            && torrent.stalled)
}

fn apply_condition(
    condition: &ConditionSpec,
    client_status: &ClientStatus,
    torrents: Vec<Torrent>,
    now: i64,
) -> Result<(Vec<Torrent>, Vec<Torrent>)> {
    match condition {
        ConditionSpec::Nothing => Ok((torrents, Vec::new())),
        ConditionSpec::Ratio(limit) => partition_by(torrents, |torrent| {
            compare(torrent.ratio, *limit, Comparer::Gt)
        }),
        ConditionSpec::CreateTime(limit) => partition_by(torrents, |torrent| {
            compare((now - torrent.create_time) as f64, *limit, Comparer::Gt)
        }),
        ConditionSpec::DownloadingTime(limit) => partition_by(torrents, |torrent| {
            compare(torrent.downloading_time as f64, *limit, Comparer::Gt)
        }),
        ConditionSpec::SeedingTime(limit) => partition_by(torrents, |torrent| {
            compare(torrent.seeding_time as f64, *limit, Comparer::Gt)
        }),
        ConditionSpec::MaxDownload(limit) => partition_by(torrents, |torrent| {
            compare(
                torrent.downloaded as f64,
                gib_to_bytes(*limit),
                Comparer::Gt,
            )
        }),
        ConditionSpec::MaxDownloadSpeed(limit) => partition_by(torrents, |torrent| {
            torrent.status == TorrentStatus::Downloading
                && compare(
                    torrent.download_speed as f64,
                    kib_to_bytes(*limit),
                    Comparer::Gt,
                )
        }),
        ConditionSpec::MinUploadSpeed(limit) => partition_by(torrents, |torrent| {
            matches!(
                torrent.status,
                TorrentStatus::Downloading | TorrentStatus::Uploading
            ) && compare(
                torrent.upload_speed as f64,
                kib_to_bytes(*limit),
                Comparer::Lt,
            )
        }),
        ConditionSpec::MaxAverageDownloadSpeed(limit) => partition_by(torrents, |torrent| {
            compare(
                torrent.average_download_speed as f64,
                kib_to_bytes(*limit),
                Comparer::Gt,
            )
        }),
        ConditionSpec::MinAverageUploadSpeed(limit) => partition_by(torrents, |torrent| {
            compare(
                torrent.average_upload_speed as f64,
                kib_to_bytes(*limit),
                Comparer::Lt,
            )
        }),
        ConditionSpec::MaxSize(limit) => partition_by(torrents, |torrent| {
            compare(torrent.size as f64, gib_to_bytes(*limit), Comparer::Gt)
        }),
        ConditionSpec::MaxSeeder(limit) => partition_by(torrents, |torrent| {
            compare(torrent.seeder as f64, *limit, Comparer::Gt)
        }),
        ConditionSpec::MaxUpload(limit) => partition_by(torrents, |torrent| {
            compare(torrent.uploaded as f64, gib_to_bytes(*limit), Comparer::Gt)
        }),
        ConditionSpec::MinLeecher(limit) => partition_by(torrents, |torrent| {
            compare(torrent.leecher as f64, *limit, Comparer::Lt)
        }),
        ConditionSpec::MaxConnectedSeeder(limit) => partition_by(torrents, |torrent| {
            matches!(
                torrent.status,
                TorrentStatus::Downloading | TorrentStatus::Uploading
            ) && compare(torrent.connected_seeder as f64, *limit, Comparer::Gt)
        }),
        ConditionSpec::MinConnectedLeecher(limit) => partition_by(torrents, |torrent| {
            matches!(
                torrent.status,
                TorrentStatus::Downloading | TorrentStatus::Uploading
            ) && compare(torrent.connected_leecher as f64, *limit, Comparer::Lt)
        }),
        ConditionSpec::LastActivity(last_activity) => partition_by(torrents, |torrent| {
            matches_last_activity(torrent, last_activity)
        }),
        ConditionSpec::MaxProgress(limit) => partition_by(torrents, |torrent| {
            compare(torrent.progress, *limit / 100.0, Comparer::Gt)
        }),
        ConditionSpec::UploadRatio(limit) => partition_by(torrents, |torrent| {
            torrent.size > 0
                && compare(
                    torrent.uploaded as f64 / torrent.size as f64,
                    *limit,
                    Comparer::Gt,
                )
        }),
        ConditionSpec::SeedSize { limit_gib, action } => {
            Ok(apply_seed_size(torrents, *limit_gib, *action))
        }
        ConditionSpec::MaximumNumber { limit, action } => {
            Ok(apply_maximum_number(torrents, *limit, *action))
        }
        ConditionSpec::FreeSpace {
            min_gib,
            path,
            action,
        } => {
            let usage = filesystem_usage(path)?;
            Ok(apply_free_space(
                torrents,
                usage.available_bytes as f64,
                *min_gib,
                *action,
            ))
        }
        ConditionSpec::RemoteFreeSpace {
            min_gib,
            path,
            action,
        } => {
            info!(path = %path, "qBittorrent ignores remote_free_space.path and uses global free space reported by the client");
            let free_space = client_status
                .free_space_on_disk
                .ok_or_else(|| anyhow!("qBittorrent did not report free_space_on_disk"))?;
            Ok(apply_free_space(
                torrents,
                free_space as f64,
                *min_gib,
                *action,
            ))
        }
        ConditionSpec::Remove(expression) => partition_by(torrents, |torrent| {
            matches_expression(expression, torrent, now)
        }),
    }
}

fn partition_by<F>(torrents: Vec<Torrent>, predicate: F) -> Result<(Vec<Torrent>, Vec<Torrent>)>
where
    F: Fn(&Torrent) -> bool,
{
    let mut remain = Vec::new();
    let mut remove = Vec::new();
    for torrent in torrents {
        if predicate(&torrent) {
            remove.push(torrent);
        } else {
            remain.push(torrent);
        }
    }
    Ok((remain, remove))
}

fn apply_seed_size(
    mut torrents: Vec<Torrent>,
    limit_gib: f64,
    action: SortAction,
) -> (Vec<Torrent>, Vec<Torrent>) {
    sort_torrents(&mut torrents, action);
    torrents.reverse();

    let mut remain = Vec::new();
    let mut remove = Vec::new();
    let mut size_sum = 0.0;
    let limit_bytes = gib_to_bytes(limit_gib);
    for torrent in torrents {
        if size_sum + (torrent.size as f64) < limit_bytes {
            size_sum += torrent.size as f64;
            remain.push(torrent);
        } else {
            remove.push(torrent);
        }
    }
    (remain, remove)
}

fn apply_maximum_number(
    mut torrents: Vec<Torrent>,
    limit: usize,
    action: SortAction,
) -> (Vec<Torrent>, Vec<Torrent>) {
    sort_torrents(&mut torrents, action);

    if limit == 0 {
        return (Vec::new(), torrents);
    }
    if limit < torrents.len() {
        let split_index = torrents.len() - limit;
        let remain = torrents.split_off(split_index);
        return (remain, torrents);
    }
    (torrents, Vec::new())
}

fn apply_free_space(
    mut torrents: Vec<Torrent>,
    current_free_space: f64,
    min_gib: f64,
    action: SortAction,
) -> (Vec<Torrent>, Vec<Torrent>) {
    sort_torrents(&mut torrents, action);

    let mut remain = Vec::new();
    let mut remove = Vec::new();
    let mut free_space = current_free_space;
    let min_bytes = gib_to_bytes(min_gib);
    for torrent in torrents {
        if free_space < min_bytes {
            free_space += torrent.size as f64;
            remove.push(torrent);
        } else {
            remain.push(torrent);
        }
    }
    (remain, remove)
}

fn sort_torrents(torrents: &mut [Torrent], action: SortAction) {
    match action {
        SortAction::RemoveOldSeeds => torrents.sort_by_key(|torrent| torrent.create_time),
        SortAction::RemoveNewSeeds => {
            torrents.sort_by(|left, right| right.create_time.cmp(&left.create_time))
        }
        SortAction::RemoveBigSeeds => torrents.sort_by(|left, right| right.size.cmp(&left.size)),
        SortAction::RemoveSmallSeeds => torrents.sort_by_key(|torrent| torrent.size),
        SortAction::RemoveActiveSeeds => {
            torrents.sort_by_key(|torrent| torrent.last_activity.unwrap_or(u64::MAX))
        }
        SortAction::RemoveInactiveSeeds => torrents.sort_by(|left, right| {
            right
                .last_activity
                .unwrap_or(0)
                .cmp(&left.last_activity.unwrap_or(0))
        }),
        SortAction::RemoveFastUploadSeeds => {
            torrents.sort_by(|left, right| right.upload_speed.cmp(&left.upload_speed))
        }
        SortAction::RemoveSlowUploadSeeds => torrents.sort_by_key(|torrent| torrent.upload_speed),
    }
}

fn matches_last_activity(torrent: &Torrent, last_activity: &LastActivityValue) -> bool {
    match last_activity {
        LastActivityValue::Seconds(limit) => torrent
            .last_activity
            .is_some_and(|last_activity| compare(last_activity as f64, *limit, Comparer::Gt)),
        LastActivityValue::Never => torrent.last_activity.is_none(),
    }
}

fn matches_expression(expression: &Expression, torrent: &Torrent, now: i64) -> bool {
    match expression {
        Expression::Relation {
            parameter,
            comparer,
            value,
        } => matches_relation(*parameter, *comparer, value, torrent, now),
        Expression::And(left, right) => {
            matches_expression(left, torrent, now) && matches_expression(right, torrent, now)
        }
        Expression::Or(left, right) => {
            matches_expression(left, torrent, now) || matches_expression(right, torrent, now)
        }
    }
}

fn matches_relation(
    parameter: Parameter,
    comparer: Comparer,
    value: &Literal,
    torrent: &Torrent,
    now: i64,
) -> bool {
    match (parameter, value) {
        (Parameter::AverageDownloadSpeed, Literal::Number(value)) => compare(
            torrent.average_download_speed as f64,
            kib_to_bytes(*value),
            comparer,
        ),
        (Parameter::AverageUploadSpeed, Literal::Number(value)) => compare(
            torrent.average_upload_speed as f64,
            kib_to_bytes(*value),
            comparer,
        ),
        (Parameter::ConnectedLeecher, Literal::Number(value)) => {
            matches!(
                torrent.status,
                TorrentStatus::Downloading | TorrentStatus::Uploading
            ) && compare(torrent.connected_leecher as f64, *value, comparer)
        }
        (Parameter::ConnectedSeeder, Literal::Number(value)) => {
            matches!(
                torrent.status,
                TorrentStatus::Downloading | TorrentStatus::Uploading
            ) && compare(torrent.connected_seeder as f64, *value, comparer)
        }
        (Parameter::CreateTime, Literal::Number(value)) => {
            compare((now - torrent.create_time) as f64, *value, comparer)
        }
        (Parameter::Download, Literal::Number(value)) => {
            compare(torrent.downloaded as f64, gib_to_bytes(*value), comparer)
        }
        (Parameter::DownloadSpeed, Literal::Number(value)) => {
            torrent.status == TorrentStatus::Downloading
                && compare(
                    torrent.download_speed as f64,
                    kib_to_bytes(*value),
                    comparer,
                )
        }
        (Parameter::DownloadingTime, Literal::Number(value)) => {
            compare(torrent.downloading_time as f64, *value, comparer)
        }
        (Parameter::LastActivity, Literal::Number(value)) => torrent
            .last_activity
            .is_some_and(|last_activity| compare(last_activity as f64, *value, comparer)),
        (Parameter::LastActivity, Literal::Text(value)) => {
            let lower = value.to_ascii_lowercase();
            (lower == "never" || lower == "none") && torrent.last_activity.is_none()
        }
        (Parameter::Leecher, Literal::Number(value)) => {
            compare(torrent.leecher as f64, *value, comparer)
        }
        (Parameter::Progress, Literal::Number(value)) => {
            compare(torrent.progress, *value / 100.0, comparer)
        }
        (Parameter::Ratio, Literal::Number(value)) => compare(torrent.ratio, *value, comparer),
        (Parameter::Seeder, Literal::Number(value)) => {
            compare(torrent.seeder as f64, *value, comparer)
        }
        (Parameter::SeedingTime, Literal::Number(value)) => {
            compare(torrent.seeding_time as f64, *value, comparer)
        }
        (Parameter::Size, Literal::Number(value)) => {
            compare(torrent.size as f64, gib_to_bytes(*value), comparer)
        }
        (Parameter::Upload, Literal::Number(value)) => {
            compare(torrent.uploaded as f64, gib_to_bytes(*value), comparer)
        }
        (Parameter::UploadRatio, Literal::Number(value)) => {
            torrent.size > 0
                && compare(
                    torrent.uploaded as f64 / torrent.size as f64,
                    *value,
                    comparer,
                )
        }
        (Parameter::UploadSpeed, Literal::Number(value)) => {
            matches!(
                torrent.status,
                TorrentStatus::Downloading | TorrentStatus::Uploading
            ) && compare(torrent.upload_speed as f64, kib_to_bytes(*value), comparer)
        }
        _ => false,
    }
}

fn compare(left: f64, right: f64, comparer: Comparer) -> bool {
    match comparer {
        Comparer::Lt => left < right,
        Comparer::Gt => left > right,
        Comparer::Eq => left == right,
    }
}

fn current_unix_timestamp_i64() -> Result<i64> {
    i64::try_from(current_unix_timestamp()?).context("unix timestamp does not fit in i64")
}

fn gib_to_bytes(gib: f64) -> f64 {
    gib * BYTES_PER_GIB
}

fn kib_to_bytes(kib: f64) -> f64 {
    kib * BYTES_PER_KIB
}

fn parse_expression(input: &str) -> Result<Expression> {
    let tokens = tokenize(input)?;
    let mut parser = ExpressionParser {
        tokens,
        position: 0,
    };
    let expression = parser.parse_expression()?;
    if parser.position != parser.tokens.len() {
        bail!("syntax error: unexpected trailing tokens in remove expression");
    }
    Ok(expression)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Identifier(String),
    Number(f64),
    Lt,
    Gt,
    Eq,
    And,
    Or,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = input.char_indices().peekable();

    while let Some((index, ch)) = chars.peek().copied() {
        match ch {
            ' ' | '\t' => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            '<' => {
                chars.next();
                tokens.push(Token::Lt);
            }
            '>' => {
                chars.next();
                tokens.push(Token::Gt);
            }
            '=' => {
                chars.next();
                tokens.push(Token::Eq);
            }
            '0'..='9' => {
                let start = index;
                chars.next();
                while let Some((_, next)) = chars.peek() {
                    if next.is_ascii_digit() || *next == '.' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                let end = chars
                    .peek()
                    .map_or(input.len(), |(next_index, _)| *next_index);
                let number = input[start..end].parse::<f64>().with_context(|| {
                    format!(
                        "invalid number in remove expression: {}",
                        &input[start..end]
                    )
                })?;
                tokens.push(Token::Number(number));
            }
            'a'..='z' | 'A'..='Z' => {
                let start = index;
                chars.next();
                while let Some((_, next)) = chars.peek() {
                    if next.is_ascii_alphanumeric() || *next == '_' || *next == '-' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                let end = chars
                    .peek()
                    .map_or(input.len(), |(next_index, _)| *next_index);
                let ident = input[start..end].to_ascii_lowercase();
                match ident.as_str() {
                    "and" => tokens.push(Token::And),
                    "or" => tokens.push(Token::Or),
                    _ => tokens.push(Token::Identifier(ident)),
                }
            }
            _ => bail!("illegal character '{}' in remove expression", ch),
        }
    }

    Ok(tokens)
}

struct ExpressionParser {
    tokens: Vec<Token>,
    position: usize,
}

impl ExpressionParser {
    fn parse_expression(&mut self) -> Result<Expression> {
        let mut expression = self.parse_primary()?;

        while let Some(token) = self.tokens.get(self.position) {
            match token {
                Token::And => {
                    self.position += 1;
                    let rhs = self.parse_primary()?;
                    expression = Expression::And(Box::new(expression), Box::new(rhs));
                }
                Token::Or => {
                    self.position += 1;
                    let rhs = self.parse_primary()?;
                    expression = Expression::Or(Box::new(expression), Box::new(rhs));
                }
                _ => break,
            }
        }

        Ok(expression)
    }

    fn parse_primary(&mut self) -> Result<Expression> {
        if matches!(self.tokens.get(self.position), Some(Token::LParen)) {
            self.position += 1;
            let expression = self.parse_expression()?;
            match self.tokens.get(self.position) {
                Some(Token::RParen) => {
                    self.position += 1;
                    return Ok(expression);
                }
                _ => bail!("syntax error: expected ')' in remove expression"),
            }
        }

        self.parse_relation()
    }

    fn parse_relation(&mut self) -> Result<Expression> {
        let parameter = match self.tokens.get(self.position) {
            Some(Token::Identifier(identifier)) => parse_parameter(identifier)?,
            Some(token) => bail!("syntax error: expected condition name, found {token:?}"),
            None => bail!("syntax error: unexpected end of remove expression"),
        };
        self.position += 1;

        let comparer = match self.tokens.get(self.position) {
            Some(Token::Lt) => Comparer::Lt,
            Some(Token::Gt) => Comparer::Gt,
            Some(Token::Eq) => Comparer::Eq,
            Some(token) => bail!("syntax error: expected comparison operator, found {token:?}"),
            None => bail!("syntax error: unexpected end of remove expression"),
        };
        self.position += 1;

        let value = match self.tokens.get(self.position) {
            Some(Token::Number(number)) => Literal::Number(*number),
            Some(Token::Identifier(identifier)) => Literal::Text(identifier.clone()),
            Some(token) => bail!("syntax error: expected literal value, found {token:?}"),
            None => bail!("syntax error: unexpected end of remove expression"),
        };
        self.position += 1;

        Ok(Expression::Relation {
            parameter,
            comparer,
            value,
        })
    }
}

fn parse_parameter(identifier: &str) -> Result<Parameter> {
    match identifier {
        "average_downloadspeed" => Ok(Parameter::AverageDownloadSpeed),
        "average_uploadspeed" => Ok(Parameter::AverageUploadSpeed),
        "connected_leecher" => Ok(Parameter::ConnectedLeecher),
        "connected_seeder" => Ok(Parameter::ConnectedSeeder),
        "create_time" => Ok(Parameter::CreateTime),
        "download" => Ok(Parameter::Download),
        "download_speed" => Ok(Parameter::DownloadSpeed),
        "downloading_time" => Ok(Parameter::DownloadingTime),
        "last_activity" => Ok(Parameter::LastActivity),
        "leecher" => Ok(Parameter::Leecher),
        "progress" => Ok(Parameter::Progress),
        "ratio" => Ok(Parameter::Ratio),
        "seeder" => Ok(Parameter::Seeder),
        "seeding_time" => Ok(Parameter::SeedingTime),
        "size" => Ok(Parameter::Size),
        "upload" => Ok(Parameter::Upload),
        "upload_ratio" => Ok(Parameter::UploadRatio),
        "upload_speed" => Ok(Parameter::UploadSpeed),
        _ => bail!("unsupported remove condition parameter: {identifier}"),
    }
}

fn parse_sort_action(action: &str) -> Result<SortAction> {
    match action.to_ascii_lowercase().as_str() {
        "remove-old-seeds" => Ok(SortAction::RemoveOldSeeds),
        "remove-new-seeds" => Ok(SortAction::RemoveNewSeeds),
        "remove-big-seeds" => Ok(SortAction::RemoveBigSeeds),
        "remove-small-seeds" => Ok(SortAction::RemoveSmallSeeds),
        "remove-active-seeds" => Ok(SortAction::RemoveActiveSeeds),
        "remove-inactive-seeds" => Ok(SortAction::RemoveInactiveSeeds),
        "remove-fast-upload-seeds" => Ok(SortAction::RemoveFastUploadSeeds),
        "remove-slow-upload-seeds" => Ok(SortAction::RemoveSlowUploadSeeds),
        _ => bail!("unsupported sort action: {action}"),
    }
}

fn parse_status_selector(item: &Item, context: &str) -> Result<StatusSelector> {
    let values = parse_string_list(item, context)?;
    let mut selector = StatusSelector::default();
    for value in values {
        match value.to_ascii_lowercase().as_str() {
            "stalledupload" => selector.stalled_upload = true,
            "stalleddownload" => selector.stalled_download = true,
            "downloading" => {
                selector.statuses.insert(TorrentStatus::Downloading);
            }
            "uploading" => {
                selector.statuses.insert(TorrentStatus::Uploading);
            }
            "checking" => {
                selector.statuses.insert(TorrentStatus::Checking);
            }
            "queued" => {
                selector.statuses.insert(TorrentStatus::Queued);
            }
            "paused" => {
                selector.statuses.insert(TorrentStatus::Paused);
            }
            "stopped" => {
                selector.statuses.insert(TorrentStatus::Stopped);
            }
            "error" => {
                selector.statuses.insert(TorrentStatus::Error);
            }
            "unknown" => {
                selector.statuses.insert(TorrentStatus::Unknown);
            }
            unknown => warn!(status = unknown, "ignoring unsupported status filter value"),
        }
    }
    Ok(selector)
}

fn parse_string_set(item: &Item, context: &str) -> Result<HashSet<String>> {
    Ok(parse_string_list(item, context)?.into_iter().collect())
}

fn parse_string_list(item: &Item, context: &str) -> Result<Vec<String>> {
    if let Some(text) = item.as_str() {
        return Ok(vec![text.to_owned()]);
    }

    let array = item
        .as_array()
        .ok_or_else(|| anyhow!("{context} must be a string or an array of strings"))?;
    let mut values = Vec::with_capacity(array.len());
    for value in array.iter() {
        let text = value
            .as_str()
            .ok_or_else(|| anyhow!("{context} must contain only strings"))?;
        values.push(text.to_owned());
    }
    Ok(values)
}

fn parse_string_item(item: &Item, context: &str) -> Result<String> {
    item.as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{context} must be a string"))
}

fn parse_bool_item(item: &Item, context: &str) -> Result<bool> {
    item.as_bool()
        .ok_or_else(|| anyhow!("{context} must be a boolean"))
}

fn parse_number_item(item: &Item, context: &str) -> Result<f64> {
    if let Some(value) = item.as_float() {
        return Ok(value);
    }
    if let Some(value) = item.as_integer() {
        return Ok(value as f64);
    }
    bail!("{context} must be a number")
}

fn get_required_string(
    table: &(impl TableLike + ?Sized),
    key: &str,
    context: &str,
) -> Result<String> {
    let item = table.get(key).ok_or_else(|| anyhow!("missing {context}"))?;
    parse_string_item(item, context)
}

fn get_optional_bool(table: &(impl TableLike + ?Sized), key: &str) -> Result<Option<bool>> {
    match table.get(key) {
        Some(item) => Ok(Some(parse_bool_item(item, key)?)),
        None => Ok(None),
    }
}

fn get_required_number(table: &(impl TableLike + ?Sized), key: &str, context: &str) -> Result<f64> {
    let item = table.get(key).ok_or_else(|| anyhow!("missing {context}"))?;
    parse_number_item(item, context)
}

fn resolve_env(value: String) -> String {
    let Some(name) = value
        .strip_prefix("$(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return value;
    };

    env::var(name).unwrap_or(value)
}

struct QbitAutoremoveClient {
    http: Client,
    base_url: Url,
    origin: String,
    referer: String,
}

impl QbitAutoremoveClient {
    fn login(config: &ClientConfig) -> Result<Self> {
        let base_url = normalize_base_url(&config.host)?;
        let origin = build_origin(&base_url)?;
        let referer = format!("{origin}/");
        let http = Client::builder()
            .cookie_store(true)
            .build()
            .context("failed to build qBittorrent HTTP client")?;

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

    fn client_status(&self) -> Result<ClientStatus> {
        let response = self.get_json::<SyncMainData>("sync/maindata")?;
        Ok(ClientStatus {
            download_speed: response.server_state.dl_info_speed,
            total_downloaded: response.server_state.dl_info_data,
            upload_speed: response.server_state.up_info_speed,
            total_uploaded: response.server_state.up_info_data,
            free_space_on_disk: response.server_state.free_space_on_disk,
        })
    }

    fn list_torrents(&self) -> Result<Vec<Torrent>> {
        let now = current_unix_timestamp()?;
        let list = self.get_json::<Vec<QbitTorrentSummary>>("torrents/info")?;
        let mut torrents = Vec::with_capacity(list.len());

        for torrent in list {
            let properties = self.get_json_with_query::<QbitTorrentProperties>(
                "torrents/properties",
                &[("hash", torrent.hash.as_str())],
            )?;
            let trackers = self.get_json_with_query::<Vec<QbitTracker>>(
                "torrents/trackers",
                &[("hash", torrent.hash.as_str())],
            )?;

            let category = if torrent.category.is_empty() {
                Vec::new()
            } else {
                vec![torrent.category]
            };
            torrents.push(Torrent {
                hash: torrent.hash,
                name: torrent.name,
                category,
                tracker: trackers.into_iter().map(|tracker| tracker.url).collect(),
                status: map_qbit_status(&torrent.state),
                stalled: matches!(torrent.state.as_str(), "stalledUP" | "stalledDL"),
                size: torrent.size,
                ratio: torrent.ratio,
                uploaded: properties.total_uploaded,
                downloaded: properties.total_downloaded,
                create_time: properties.addition_date,
                seeding_time: properties.seeding_time,
                downloading_time: properties.downloading_time,
                upload_speed: properties.up_speed,
                average_upload_speed: properties.up_speed_avg,
                download_speed: properties.dl_speed,
                average_download_speed: properties.dl_speed_avg,
                last_activity: torrent
                    .last_activity
                    .filter(|last_activity| *last_activity > 0)
                    .map(|last_activity| now.saturating_sub(last_activity as u64)),
                seeder: properties.seeds_total,
                connected_seeder: properties.seeds,
                leecher: properties.peers_total,
                connected_leecher: properties.peers,
                progress: torrent.progress,
            });
        }

        Ok(torrents)
    }

    fn delete_torrents(&self, hashes: &[String], delete_data: bool) -> Result<()> {
        if hashes.is_empty() {
            return Ok(());
        }

        let url = api_url(&self.base_url, "torrents/delete")?;
        let response = self
            .http
            .post(url)
            .header(REFERER, &self.referer)
            .header(ORIGIN, &self.origin)
            .form(&[
                ("hashes", hashes.join("|")),
                (
                    "deleteFiles",
                    if delete_data { "true" } else { "false" }.to_owned(),
                ),
            ])
            .send()
            .context("failed to call qBittorrent delete endpoint")?;

        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let body = response
            .text()
            .context("failed to read qBittorrent delete error body")?;
        bail!(
            "qBittorrent delete failed with status {}: {}",
            status,
            body.trim()
        )
    }

    fn get_json<T>(&self, endpoint: &str) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = api_url(&self.base_url, endpoint)?;
        let response = self
            .http
            .get(url)
            .header(REFERER, &self.referer)
            .send()
            .with_context(|| format!("failed to call qBittorrent endpoint {endpoint}"))?;

        let status = response.status();
        if !status.is_success() {
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

        response
            .json::<T>()
            .with_context(|| format!("failed to decode qBittorrent JSON for {endpoint}"))
    }

    fn get_json_with_query<T>(&self, endpoint: &str, query: &[(&str, &str)]) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let url = api_url(&self.base_url, endpoint)?;
        let response = self
            .http
            .get(url)
            .header(REFERER, &self.referer)
            .query(query)
            .send()
            .with_context(|| format!("failed to call qBittorrent endpoint {endpoint}"))?;

        let status = response.status();
        if !status.is_success() {
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

        response
            .json::<T>()
            .with_context(|| format!("failed to decode qBittorrent JSON for {endpoint}"))
    }
}

#[derive(Debug, Deserialize)]
struct SyncMainData {
    server_state: SyncServerState,
}

#[derive(Debug, Deserialize)]
struct SyncServerState {
    #[serde(default)]
    dl_info_speed: u64,
    #[serde(default)]
    dl_info_data: u64,
    #[serde(default)]
    up_info_speed: u64,
    #[serde(default)]
    up_info_data: u64,
    #[serde(default)]
    free_space_on_disk: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct QbitTorrentSummary {
    hash: String,
    name: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    state: String,
    size: u64,
    ratio: f64,
    progress: f64,
    #[serde(default)]
    last_activity: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct QbitTorrentProperties {
    total_uploaded: i64,
    total_downloaded: i64,
    addition_date: i64,
    #[serde(default)]
    seeding_time: i64,
    #[serde(default)]
    downloading_time: i64,
    #[serde(default)]
    up_speed: i64,
    #[serde(default)]
    dl_speed: i64,
    #[serde(default)]
    seeds_total: i64,
    #[serde(default)]
    seeds: i64,
    #[serde(default)]
    peers_total: i64,
    #[serde(default)]
    peers: i64,
    #[serde(default)]
    up_speed_avg: i64,
    #[serde(default)]
    dl_speed_avg: i64,
}

#[derive(Debug, Deserialize)]
struct QbitTracker {
    url: String,
}

fn map_qbit_status(state: &str) -> TorrentStatus {
    match state {
        "downloading" | "stalledDL" => TorrentStatus::Downloading,
        "queuedDL" | "queuedUP" => TorrentStatus::Queued,
        "uploading" | "stalledUP" => TorrentStatus::Uploading,
        "checkingUP" | "checkingDL" => TorrentStatus::Checking,
        "pausedUP" | "pausedDL" => TorrentStatus::Paused,
        "error" => TorrentStatus::Error,
        _ => TorrentStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::{Array, Value};

    fn torrent(name: &str) -> Torrent {
        Torrent {
            hash: format!("hash-{name}"),
            name: name.to_string(),
            category: vec![],
            tracker: vec![],
            status: TorrentStatus::Uploading,
            stalled: false,
            size: 100,
            ratio: 1.0,
            uploaded: 100,
            downloaded: 100,
            create_time: 100,
            seeding_time: 100,
            downloading_time: 100,
            upload_speed: 100,
            average_upload_speed: 100,
            download_speed: 100,
            average_download_speed: 100,
            last_activity: Some(100),
            seeder: 10,
            connected_seeder: 5,
            leecher: 5,
            connected_leecher: 3,
            progress: 1.0,
        }
    }

    #[test]
    fn parses_strategy_condition_order_from_toml() {
        let document = r#"
[client]
host = "http://127.0.0.1:8080"
username = "admin"
password = "secret"

[[tasks]]
name = "cleanup"

  [[tasks.strategies]]
  name = "ordered"
  categories = "IPT"
  ratio = 1
  remove = "seeding_time > 10"
  maximum_number = { limit = 5, action = "remove-old-seeds" }
"#
        .parse::<DocumentMut>()
        .expect("valid document");

        let tasks = parse_tasks(&document).expect("tasks parse");
        let strategy = &tasks[0].strategies[0];
        assert!(matches!(strategy.conditions[0], ConditionSpec::Ratio(_)));
        assert!(matches!(strategy.conditions[1], ConditionSpec::Remove(_)));
        assert!(matches!(
            strategy.conditions[2],
            ConditionSpec::MaximumNumber { .. }
        ));
    }

    #[test]
    fn excluded_only_filters_default_to_all() {
        let mut filters = FilterConfig {
            excluded_categories: HashSet::from(["skip".to_string()]),
            all_categories: true,
            all_trackers: true,
            all_status: true,
            ..FilterConfig::default()
        };
        let mut keep = torrent("keep");
        keep.category = vec!["keep".to_string()];
        let mut skip = torrent("skip");
        skip.category = vec!["skip".to_string()];

        let filtered = apply_filters(&filters, &[keep.clone(), skip]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, keep.name);

        filters.all_categories = false;
        let filtered = apply_filters(&filters, &[keep]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn status_selector_understands_stalled_tokens() {
        let selector = parse_status_selector(
            &Item::Value(Value::Array(Array::from_iter([
                "Uploading",
                "StalledDownload",
            ]))),
            "status",
        )
        .expect("selector parses");

        assert!(selector.statuses.contains(&TorrentStatus::Uploading));
        assert!(selector.stalled_download);
    }

    #[test]
    fn remove_expression_is_left_associative() {
        let expression = parse_expression("ratio > 1 or seeding_time > 50 and create_time > 500")
            .expect("expression parses");
        let now = 1_000;

        let mut sample = torrent("sample");
        sample.ratio = 2.0;
        sample.seeding_time = 10;
        sample.create_time = 900;

        assert!(!matches_expression(&expression, &sample, now));

        sample.create_time = 100;
        assert!(matches_expression(&expression, &sample, now));
    }

    #[test]
    fn last_activity_none_matches_never_active_torrents() {
        let expression = parse_expression("last_activity = none").expect("expression parses");
        let mut active = torrent("active");
        active.last_activity = Some(10);
        let mut never = torrent("never");
        never.last_activity = None;

        assert!(!matches_expression(&expression, &active, 1_000));
        assert!(matches_expression(&expression, &never, 1_000));
    }

    #[test]
    fn qbit_properties_accept_negative_sentinel_values() {
        let properties: QbitTorrentProperties = serde_json::from_str(
            r#"{
                "total_uploaded": -1,
                "total_downloaded": -1,
                "addition_date": -1,
                "seeding_time": -1,
                "downloading_time": -1,
                "up_speed": -1,
                "dl_speed": -1,
                "seeds_total": -1,
                "seeds": -1,
                "peers_total": -1,
                "peers": -1,
                "up_speed_avg": -1,
                "dl_speed_avg": -1
            }"#,
        )
        .expect("properties deserialize");

        assert_eq!(properties.total_uploaded, -1);
        assert_eq!(properties.addition_date, -1);
        assert_eq!(properties.seeding_time, -1);
        assert_eq!(properties.dl_speed_avg, -1);
    }

    #[test]
    fn create_time_condition_preserves_negative_qbit_values() {
        let expression = parse_expression("create_time > 1000").expect("expression parses");
        let mut sample = torrent("sample");
        sample.create_time = -1;

        assert!(matches_expression(&expression, &sample, 1_000));
    }

    #[test]
    fn remove_old_seeds_sorts_negative_create_time_first() {
        let mut unknown = torrent("unknown");
        unknown.create_time = -1;
        let mut known = torrent("known");
        known.create_time = 10;

        let (_remain, remove) = apply_maximum_number(
            vec![known.clone(), unknown.clone()],
            1,
            SortAction::RemoveOldSeeds,
        );

        assert_eq!(remove.len(), 1);
        assert_eq!(remove[0].name, unknown.name);
    }

    #[test]
    fn seed_size_remove_big_seeds_keeps_smallest_first() {
        let mut small = torrent("small");
        small.size = 100;
        let mut medium = torrent("medium");
        medium.size = 200;
        let mut big = torrent("big");
        big.size = 400;

        let (remain, remove) = apply_seed_size(
            vec![small.clone(), medium.clone(), big.clone()],
            0.0000003,
            SortAction::RemoveBigSeeds,
        );

        assert_eq!(remain.len(), 2);
        assert_eq!(remove.len(), 1);
        assert_eq!(remove[0].name, big.name);
        assert_eq!(remain[0].name, small.name);
        assert_eq!(remain[1].name, medium.name);
    }

    #[test]
    fn maximum_number_remove_old_seeds_removes_oldest_first() {
        let mut oldest = torrent("oldest");
        oldest.create_time = 10;
        let mut middle = torrent("middle");
        middle.create_time = 20;
        let mut newest = torrent("newest");
        newest.create_time = 30;

        let (remain, remove) = apply_maximum_number(
            vec![middle.clone(), newest.clone(), oldest.clone()],
            2,
            SortAction::RemoveOldSeeds,
        );

        assert_eq!(remove.len(), 1);
        assert_eq!(remove[0].name, oldest.name);
        assert_eq!(remain.len(), 2);
    }

    #[test]
    fn task_client_values_support_env_substitution() {
        let key = "QB_AUTOREMOVE_TEST_SECRET";
        let expected = "supersecret";
        unsafe {
            std::env::set_var(key, expected);
        }
        assert_eq!(resolve_env(format!("$({key})")), expected);
        assert_eq!(resolve_env("plain".to_string()), "plain");
    }

    #[test]
    fn run_all_tasks_continues_after_failures() {
        let tasks = vec![
            AutoremoveTask {
                name: "first".to_string(),
                delete_data: false,
                strategies: Vec::new(),
            },
            AutoremoveTask {
                name: "second".to_string(),
                delete_data: false,
                strategies: Vec::new(),
            },
        ];
        let mut seen = Vec::new();

        let failed = run_all_tasks(&tasks, |task| {
            seen.push(task.name.clone());
            if task.name == "first" {
                Err(anyhow::anyhow!("boom"))
            } else {
                Ok(())
            }
        });

        assert_eq!(failed, 1);
        assert_eq!(seen, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn delete_candidates_keep_first_matching_strategy() {
        let mut remove_by_hash = HashMap::new();
        let sample = torrent("sample");

        record_delete_candidate(&mut remove_by_hash, sample.clone(), "first");
        record_delete_candidate(&mut remove_by_hash, sample, "second");

        let candidate = remove_by_hash
            .get("hash-sample")
            .expect("candidate recorded");
        assert_eq!(remove_by_hash.len(), 1);
        assert_eq!(candidate.torrent.name, "sample");
        assert_eq!(candidate.strategy, "first");
    }
}
