# <img src="assets/icon.png" width="44" align="top"> gpudash

**Keep track of lots of Slurm jobs, submitted by lots of agents, across all your clusters, from one terminal.**

When several agents (or sweeps, or teammates' scripts) are submitting jobs to several
clusters at once, you can't watch them all. gpudash shows every job on every cluster in one
live view, and your Mac tells you when jobs are queued, start, finish or fail.

![gpudash: jobs from two clusters in one view](assets/screenshots/main.png)

## Features

### Every job, every cluster, one screen

Running, queued and recently ended jobs from all your clusters, refreshed every 30 seconds.
Queued jobs show Slurm's estimated start time, in your time zone. Ended jobs show how they
ended: `✓ done`, `✗ fail`, `✗ oom`, `⏱ time`, `⊘ cncl`.

### Notifications, so you don't have to watch

![example gpudash notifications (queued, started, failed)](assets/screenshots/notifications.png)

A notification when jobs are **queued**, **start**, **finish**, **fail**, run **out of
memory** or hit their **time limit**. When an agent submits a batch at once, you get one
notification ("6 jobs queued on explorer"), not six. Each kind can be turned off.

**Keeps working after you close it.** After the first run, gpudash keeps polling in the
background (every 5 minutes) and notifies you even when the dashboard isn't open, including
short jobs that started and finished between two checks. If many jobs end while your laptop
sleeps, you get one summary ("12 jobs ended on explorer: 10 done · 2 failed"). Turn it off
with `gpudash background off` or `p` → *Background notifications*.

### Live GPU readings

![usage and memory bars with a temperature graph](assets/screenshots/detail.png)

Every running job shows each GPU's temperature (blue when cool, red when hot). The selected
job gets big usage and memory bars and a temperature graph.

### Logs in one keypress

![a colored job log](assets/screenshots/log.png)

`enter` opens the job's log: colored, scrollable, and following the end while the job runs.
It finds logs even after Slurm has forgotten the job. Unchanged logs stay on screen without
being downloaded again; `r` forces a fresh read.

### Command palette

![the command palette](assets/screenshots/palette.png)

`p` opens every action and setting in one searchable list. Changes are saved for next time.

### Themes

![four of the built-in themes](assets/screenshots/themes.png)

Six built-in themes (`default`, `light`, `nord`, `gruvbox`, `catppuccin-mocha`,
`tokyo-night`). Pick one with `p` → *Theme*; moving through the list previews each theme
live, `enter` keeps it and `esc` goes back. Make your own by adding a
`.toml` file to `~/.config/gpudash/themes/`; see [Themes](#making-a-theme) below.

### And

- `s` opens a shell **inside a running job**, on its GPU node.
- `x` cancels a job (asks first). `/` filters by name, folder or id.
- One shared ssh connection per cluster keeps refreshes quick. Each cluster refreshes on
  its own timer, so one that stops answering shows an error and retries without holding up the rest.

## Install

You need [Rust](https://rustup.rs) and, on a Mac, Apple's Command Line Tools
(`xcode-select --install` if you don't have them).

```sh
cargo install --git https://github.com/RollingRo11/gpudash
```

**Updating:** pulling the repo isn't enough, reinstall to rebuild:

```sh
cargo install --git https://github.com/RollingRo11/gpudash --force   # or, in a clone: git pull && cargo install --path . --force
```

`gpudash --version` shows which commit you're running.

The first time you open gpudash it also registers itself to start at login and poll in
the background (`gpudash background status` shows it, the log is `~/Library/Logs/gpudash.log`).

On macOS this also builds a small notification app that puts the gpudash icon on
notifications. It's set up the first time a notification is sent; click **Allow** when
macOS asks.

## Setup

**1. ssh.** Each cluster needs a `Host` entry in `~/.ssh/config` that logs in without a
password. Check with `ssh -o BatchMode=yes mycluster true`.

**2. Clusters.** Start with one, then add the rest from inside the app (`p` → *Add cluster*):

```sh
gpudash explorer
```

gpudash saves them, so next time plain `gpudash` shows them all.

## Keys

| key           | action                                          |
| ------------- | ----------------------------------------------- |
| `p`           | command palette                                 |
| `j` `k` / `↑` `↓` | move                                        |
| `enter` / `l` | open the job's log (`e` stdout/stderr, `h` / `esc` back) |
| `s`           | shell into the job                              |
| `x`           | cancel the job                                  |
| `y`           | copy the job id                                 |
| `/`           | filter                                          |
| `e`           | ended jobs: recent → all from 24h → hidden      |
| `n`           | notifications on / off                          |
| `r`           | refresh now                                     |
| `?`           | all keys                                        |
| `q`           | quit                                            |
| left click    | select a job, or run a command palette item      |
| scroll wheel  | move, or scroll the log                         |

gpudash uses the mouse, so to select text in the terminal hold **Shift** while dragging.

## Configuration

Settings live in `~/.config/gpudash/config.toml`. The command palette changes them for you;
these are the defaults:

```toml
theme = "default"           # any file in ~/.config/gpudash/themes/
refresh_seconds = 30        # while the dashboard is open
background = true           # keep notifying after it closes (macOS)
background_seconds = 300    # how often to poll in the background
ended_jobs = "recent"       # "recent", "all" or "hidden"
recent_ended = 5
notifications = true
notify_on_queue = true
notify_on_start = true
notify_sound = false
temperature_unit = "C"      # or "F"
clock = "12h"               # or "24h"
temperature_graph = true

[[cluster]]
host = "explorer"           # ssh Host name
prefix = "/projects/me/"    # optional: hidden from the folder column
```

### Making a theme

On first run gpudash copies its built-in themes to `~/.config/gpudash/themes/`. Copy one,
change the colors, and pick it with `p` → *Theme* (picking it again reloads your edits).
Any key you leave out keeps the default color:

```toml
# ~/.config/gpudash/themes/mine.toml
background = ""                 # "" keeps your terminal's background
text = "#d7dce6"
accent = "#78aaff"
running = "#50dc8c"
queued = "#f0c850"
failed = "#ff5050"
temperature = ["#5aa0ff", "#50dc8c", "#f0dc50", "#ff8c32", "#ff323c"]  # cool → hot
util = ["#ff7d46", "#ff3ca0"]   # usage bar
memory = ["#ffd764", "#ff915f"] # memory bar

[log]
number = "#d7a0ff"
error = "#ff9696"
```

The full list of keys is in [`themes/default.toml`](themes/default.toml).

`gpudash mycluster` watches just that cluster; `gpudash -n 60` refreshes every 60 s this time.

## How it works

Each refresh sends small scripts to each cluster's login node over ssh:

- `squeue` for active jobs, `sacct` for jobs that ended in the last 24 hours
- then, in a second script, `srun --jobid=<id> --overlap nvidia-smi` inside each running job,
  to read its GPUs (no new resources used). Job states show up before the GPU readings.

Notifications come from a tiny Swift app ([`notifier/main.swift`](notifier/main.swift)),
because macOS shows the Script Editor icon on anything sent with `osascript`.

In the background, a launchd agent (`~/Library/LaunchAgents/io.github.rollingro11.gpudash.plist`)
runs `gpudash daemon`. While the dashboard is open it stands by and the dashboard notifies;
both share a "last seen" snapshot per cluster in `~/.cache/gpudash/`, so nothing is missed or
sent twice when one hands over to the other.

## Requirements

- **Clusters:** Slurm 20.11 or newer, `nvidia-smi` on GPU nodes. Tested on Slurm 23.11 and 26.05.
- **Laptop:** macOS or Linux. Notifications and copying are macOS only.
- Only the first node of a multi-node job is read for GPU stats.

## License

MIT
