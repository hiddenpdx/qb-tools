# qb-tools

Move completed qBittorrent torrents from a fast drive to a slower drive after they have been finished for a configured number of days.

The tool talks to the qBittorrent WebUI API directly, so you only need the compiled Rust binary on the machine that runs it.

This crate now builds three binaries:

- `qb-move-after-days`
- `qb-move-on-low-space`
- `qb-autoremove`

## Features

- qBittorrent only
- TOML config
- Multiple move rules with different source folders, targets, and age thresholds
- Optional per-rule low-space thresholds
- Separate autoremove tasks and strategies that delete torrents through qBittorrent
- Autoremove-style filters, keyword conditions, and boolean `remove` expressions
- Content-path-aware cross-seed handling for move and autoremove flows
- Dry-run mode
- Console logging on every run
- Optional file logs, with shared rotating or single-file mode in config and `--log <folder>` choosing the directory
- Disables qBittorrent Automatic Torrent Management on a torrent before moving it
- Preserves the relative save path from the source root to the target root

## Requirements

- qBittorrent WebUI enabled
- A qBittorrent user with permission to move torrent data
- qBittorrent must have write access to both the source and target storage
- Rust only for building the binary

## Build

```bash
cargo build --release
```

The binaries will be available at:

- `target/release/qb-move-after-days`
- `target/release/qb-move-on-low-space`
- `target/release/qb-autoremove`

## Cargo Dist

This repo is configured with [Cargo Dist](https://github.com/axodotdev/cargo-dist) for GitHub Releases.

- Dist package name: `qb-tools`
- Release bundle contains:
  - `qb-move-after-days`
  - `qb-move-on-low-space`
  - `qb-autoremove`
- Release targets:
  - `x86_64-unknown-linux-gnu`
  - `aarch64-unknown-linux-gnu`
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
- Installer type:
  - none

Useful commands:

```bash
dist plan
dist build --target x86_64-unknown-linux-musl
```

Release CI is generated in `.github/workflows/release.yml` and is triggered by pushing a version tag such as `v0.1.0`.

## Docker Publishing

Release builds also publish multi-arch Docker images for the Linux binaries. Cargo Dist now emits both Linux `gnu` and `musl` archives, while Docker builds only the `musl` binaries in a dedicated CI build phase and then assembles Alpine images without compiling Rust inside Docker.

- Docker runtime image: Alpine via `Dockerfile`
- Docker build workflow: `.github/workflows/docker-build.yml`
- Docker publish workflow: `.github/workflows/docker-publish.yml`
- Docker Hub images:
  - `hiddenpdx/qb-autoremove`
  - `hiddenpdx/qb-move-after-days`
  - `hiddenpdx/qb-move-on-low-space`
- Required GitHub secrets:
  - `DOCKERHUB_USERNAME`
  - `DOCKERHUB_TOKEN`
- Tags published per release:
  - always the exact release tag, such as `v0.1.0`
  - `latest` only for non-prerelease tags
- Prerelease tags skip both the Docker build phase and Docker publish phase
- Container default command: `--daemon --config /config/config.toml`

The Docker jobs are wired into Cargo Dist via `local-artifacts-jobs` and `publish-jobs`, so updating dist-managed CI should be done with `dist generate --mode ci`.

## Config

Create a TOML config file:

```toml
[client]
host = "http://127.0.0.1:8080"
username = "admin"
password = "secret"

[logging]
mode = "rotating"

[[rules]]
source_path = "/Volumes/SSD/Movies"
target_path = "/Volumes/HDD/Movies"
min_days_since_completion = 14
min_free_space_percent = 12.5

[[rules]]
source_path = "/Volumes/SSD/TV"
target_path = "/Volumes/HDD/TV"
min_days_since_completion = 7
min_free_space_percent = 15.0

[[tasks]]
name = "ipt_cleanup"
delete_data = true

[[tasks.strategies]]
name = "ratio_or_seedtime"
categories = "IPT"
remove = "seeding_time > 1209600 or ratio > 1"

[[tasks.strategies]]
name = "recover_space"
all_categories = true
free_space = { min = 10.0, path = "/Volumes/SSD", action = "remove-big-seeds" }
```

A ready-to-edit example is included as `config.example.toml`.

- `[[rules]]` are used by `qb-move-after-days` and `qb-move-on-low-space`
- `[[tasks]]` are used by `qb-autoremove`
- All binaries share the same `[client]` section
- All binaries share the same optional `[logging]` section

## Rule Matching

- A rule matches when a torrent's `save_path` is under the rule's `source_path`
- If multiple rules match, the most specific one wins
- Example: `/Volumes/SSD/Movies` wins over `/Volumes/SSD`

## Path Mapping

The relative path under the source root is preserved when moving:

- `/Volumes/SSD/Movies` -> `/Volumes/HDD/Movies`
- `/Volumes/SSD/TV/Show` -> `/Volumes/HDD/TV/Show`

## Cross-Seed Handling

These tools treat torrents that share the same qBittorrent `content_path` as one cross-seeded group.

- `qb-move-after-days` moves every torrent in the group to the same destination
- `qb-move-on-low-space` counts the group once for reclaimable size and queues every torrent in the group to the same destination
- `qb-autoremove` expands a selected torrent into the full `content_path` group before deleting it
- If `content_path` is empty or unavailable, the torrent is treated as standalone

For `qb-autoremove`, `create_time` is group-aware:

- `create_time` conditions use the earliest `addition_date` in the cross-seeded group
- `remove` expressions that reference `create_time` use that same earliest timestamp
- `remove-old-seeds` and `remove-new-seeds` sort by the earliest group timestamp
- Other metrics such as ratio, seeding time, downloading time, size, activity, speeds, and peers remain per-torrent

## Usage

`qb-move-after-days` uses `min_days_since_completion`.

Run a dry-run first:

```bash
target/release/qb-move-after-days --config /path/to/config.toml --dry-run
```

Run for real:

```bash
target/release/qb-move-after-days --config /path/to/config.toml
```

Enable file logs as well:

```bash
target/release/qb-move-after-days --config /path/to/config.toml --log /path/to/logs
```

Run continuously in the foreground, checking every 10 minutes:

```bash
target/release/qb-move-after-days --config /path/to/config.toml --daemon --log /path/to/logs
```

Run continuously with a custom interval in seconds:

```bash
target/release/qb-move-after-days --config /path/to/config.toml --daemon --interval 300
```

`qb-move-on-low-space` uses `min_free_space_percent` on each rule.

Run a dry-run to see which torrents would be queued when a source filesystem is below its threshold:

```bash
target/release/qb-move-on-low-space --config /path/to/config.toml --dry-run
```

Run it for real:

```bash
target/release/qb-move-on-low-space --config /path/to/config.toml --log /path/to/logs
```

Run continuously in the foreground, checking every 10 minutes:

```bash
target/release/qb-move-on-low-space --config /path/to/config.toml --daemon --log /path/to/logs
```

Run continuously with a custom interval in seconds:

```bash
target/release/qb-move-on-low-space --config /path/to/config.toml --daemon --interval 300
```

`qb-autoremove` uses `[[tasks]]` and deletes torrents instead of moving them.

- If `--config` is omitted, it reads `config.toml` from the current directory
- File logs are only enabled when `--log <folder>` is provided
- When running all tasks, task failures are logged and later tasks still run
- `--daemon` keeps `qb-autoremove` running in the foreground and repeats the check every `--interval` seconds
- `--interval <SECONDS>` is only valid with `--daemon` and defaults to `600`
- Daemon mode loads config once at startup; restart the process after changing config or logging settings

Both move binaries support the same daemon flags:

- `--daemon` keeps the binary running in the foreground and repeats the check every `--interval` seconds
- `--interval <SECONDS>` is only valid with `--daemon` and defaults to `600`
- Move daemon mode also loads config once at startup; restart the process after changing config or logging settings

Preview one task without deleting anything:

```bash
target/release/qb-autoremove --config /path/to/config.toml --task ipt_cleanup --view
```

Run all autoremove tasks for real:

```bash
target/release/qb-autoremove --config /path/to/config.toml --log /path/to/logs
```

Run continuously in the foreground, checking every 10 minutes:

```bash
target/release/qb-autoremove --config /path/to/config.toml --daemon --log /path/to/logs
```

Run continuously with a custom interval in seconds:

```bash
target/release/qb-autoremove --config /path/to/config.toml --daemon --interval 300 --task ipt_cleanup
```

Enable debug logging:

```bash
target/release/qb-autoremove --config /path/to/config.toml --debug
```

Run with only defaults from the current directory:

```bash
target/release/qb-autoremove --view
```

## Autoremove Tasks

- Each `[[tasks]]` entry has a required `name` and optional `delete_data = true`
- Each `[[tasks.strategies]]` entry has a required `name`
- Strategy filters are evaluated per torrent before any cross-seed expansion
- Strategy filters support:
  - `all_categories`, `categories`, `excluded_categories`
  - `all_trackers`, `trackers`, `excluded_trackers`
  - `all_status`, `status`, `excluded_status`
- Single-item filters can be written as a string instead of an array
- Conditions are applied in the order they appear in the TOML strategy
- Supported keyword conditions include:
  - `ratio`, `create_time`, `downloading_time`, `seeding_time`
  - `max_download`, `max_downloadspeed`, `min_uploadspeed`
  - `max_average_downloadspeed`, `min_average_uploadspeed`
  - `max_size`, `max_seeder`, `max_upload`, `min_leecher`
  - `max_connected_seeder`, `min_connected_leecher`
  - `last_activity`, `max_progress`, `upload_ratio`
  - `seed_size`, `maximum_number`, `free_space`, `remote_free_space`, `nothing`
- `remove = "..."` supports `and`, `or`, `(`, `)`, `<`, `>`, and `=`
- `and` and `or` have the same precedence and are left-associative, matching upstream `autoremove-torrents`
- For qBittorrent, `remote_free_space.path` is accepted for compatibility but ignored; qBittorrent reports one global free-space value
- After a torrent passes a strategy's remove conditions, `qb-autoremove` adds every torrent in the same `content_path` group to the delete set
- Cross-seeded siblings are deduplicated across strategies, so the same torrent is only deleted once

## Logging Config

- `[logging]` is optional
- `mode = "rotating"` is the default and uses a daily rotated logfile when file logging is enabled
- `mode = "single"` writes to one persistent logfile per binary when file logging is enabled
- File logging is disabled unless you pass `--log <folder>`
- `--log` only chooses the directory
- Log filenames are:
  - `qb-move-after-days.log`
  - `qb-move-on-low-space.log`
  - `qb-autoremove.log`

## Low-Space Behavior

- Each `[[rules]]` entry can set `min_free_space_percent`
- The binary checks free space on the filesystem that contains `source_path`
- If free space is below the configured threshold, it finds completed qBittorrent torrents for that rule
- It sorts them by qBittorrent `completion_on`, oldest first
- It treats torrents that share the same `content_path` as one group and counts that group once for reclaimable size
- It queues enough torrent groups to cover the current free-space deficit based on group size
- When a group is selected, it queues every torrent in that group to the same destination
- It waits for those moves to finish, checks free space again, and repeats if more space is still needed
- If `min_free_space_percent` is omitted for a rule, `qb-move-on-low-space` ignores that rule

## Logging

- Console logging is always enabled
- `--log <folder>` adds file logs in that folder
- `[logging].mode = "rotating"` uses daily rotation
- `[logging].mode = "single"` uses one persistent logfile
- Each run logs:
  - loaded rules
  - skipped torrents and why they were skipped
  - dry-run candidates
  - torrents that had `auto_tmm` disabled
  - move requests and failures
  - a final summary

`qb-autoremove` also logs cross-seed details:

- selected primaries log `content_path`, `effective_create_time`, and `cross_seeds_found`
- dry-run, delete, and delete-failure lines log `group_primary`, `group_primary_hash`, and `cross_seeds_found`
- each `strategy finished` line reports selected-scope cross-seed stats:
  - `selected_primaries`
  - `cross_seed_groups`
  - `cross_seeds_found`
  - `unique_candidates_added`
- the final `autoremove task finished` line reports filtered-scope cross-seed presence:
  - `cross_seed_groups` is the number of filtered `content_path` groups with at least one sibling
  - `cross_seeds_found` is the total number of sibling torrents present across those filtered groups

## Automatic Torrent Management

If a matching torrent is using qBittorrent Automatic Torrent Management, the tool disables it for that torrent before requesting the move. This avoids qBittorrent moving the torrent back to a category-managed path later.

## Scheduling

Example cron entry that runs every hour:

```cron
0 * * * * /path/to/qb-move-after-days --config /path/to/config.toml --log /path/to/logs
```

## Common Failure Cases

- qBittorrent WebUI URL, username, or password is wrong
- qBittorrent cannot write to the destination drive
- The destination path cannot be created by qBittorrent
- A torrent is already in qBittorrent's `moving` state
- The torrent save path does not match any configured rule

## Manual Files

This tool does not move files itself. It only tells qBittorrent to move a torrent.

- Files outside the torrent's actual data are not moved just because they are in the same source folder
- Files inside the torrent's content directory may move along with that torrent, because qBittorrent is moving that directory

In practice:

- A sibling file like `/Volumes/SSD/Movies/note.txt` will usually not move when a torrent under `/Volumes/SSD/Movies/Some.Movie/` is moved
- A manual file placed inside `/Volumes/SSD/Movies/Some.Movie/` may move, because it is inside the directory qBittorrent moves
