# CodeTime for OpenIDEA

`inf1mum.codetime` — privacy-first coding-time tracking for
[OpenIDEA](https://github.com/l0ng-ai/openidea): heartbeats from the editor's
file events, today's total in the status bar, and a dashboard for today and
the last 7 days. All data stays in the plugin's local storage on your machine;
there is no network, no account, no telemetry.

## What it does

- **Heartbeats.** Opening, saving or focusing a file is a heartbeat, and a
  heartbeat credits the stretch of time since the previous one to the file
  that stretch was spent on — the previously focused one, so the work before
  a save, a switch to another file or losing editor focus counts. A stretch
  longer than the idle threshold is treated as idle and counts nothing; the
  heartbeat that ends it starts a new session.
- **Idle detection.** After `idleMinutes` (default 5) without a heartbeat,
  time stops counting until the next one.
- **Across reclaims.** The host reclaims a plugin's instance after a few
  minutes of idleness and recreates it on demand. CodeTime persists its
  accounting session (which file, since when) and restores it on activation,
  so a stretch that spans a reclaim still settles to the right file at the
  next heartbeat. The manifest's `on-schedule` adds a wakeup for the same
  purpose: the host delivers `Scheduled` only to an instance it recreated
  after reclaiming the idle one — never to a live one — so it is not a
  minute-by-minute counter. It settles a quiet stretch once while it is
  still within the idle threshold, and expires the session otherwise; at the
  default 5 minutes (the same as the host's reclaim timeout) a quiet stretch
  is already past the threshold when the wakeup arrives and counts nothing.
- **Status bar.** Today's total as `H:MM`; the tooltip breaks it down by
  project. Clicking it opens the dashboard.
- **Dashboard.** A bottom tool window with a table of your time by project,
  by language and the files with the most time — for today or the last 7
  days (toolbar buttons).
- **Commands** (search for "CodeTime"):
  - *CodeTime: Show Dashboard* — opens the dashboard;
  - *CodeTime: Export Summary to Clipboard* — a text summary of today and
    the week (by project and language);
  - *CodeTime: Reset Statistics* — deletes every recorded day and ends the
    current accounting session, so counting starts from scratch (the stretch
    of work since the last heartbeat is not carried over).

## Install

From the community marketplace (Settings › Plugins › Available › search
"CodeTime"), or download `inf1mum.codetime-0.1.2.oiplugin` from
[Releases](https://github.com/Inf1mum/openidea-codetime/releases) and use
Settings › Plugins › *Install Plugin from Disk…*.

## Settings

Settings › Plugins › CodeTime:

| Setting | Meaning |
| --- | --- |
| `idleMinutes` | Minutes without a heartbeat after which counting pauses (1–240, default 5). |
| `exclude` | Globs of paths whose time is never counted, e.g. `**/node_modules/**`. `*` stays inside one path segment, `**` crosses segments, `?` is one character; a pattern without `/` matches the file name alone, a pattern with `/` is anchored to the path root. |

## Capabilities and why

The manifest requires `events`, `project.read` and `storage`, and optionally
`clipboard`. CodeTime asks for nothing else — in particular no `network:*`,
no `process:*`, no `events.input`.

| Capability | Risk | Why CodeTime needs it |
| --- | --- | --- |
| `events` | Low | Subscribes to `file-opened`, `file-saved` and `editor-focused` — the heartbeats time is counted from. |
| `project.read` | Low | Resolves which open project a heartbeat's file belongs to, so the dashboard can group time per project. |
| `storage` | Low | Persists the per-day buckets the status bar and dashboard read; survives restarts and idle reclaim. |
| `clipboard` *(optional)* | Medium | Only *Export Summary to Clipboard* writes to the system clipboard. Without it, every other feature works and the export command explains what is missing. |

## Honest limits

- **No keystrokes, no mouse.** The plugin API only exposes
  `events.input`-less file events here, so coding time is *approximated*
  from saves, focus switches and losing editor focus; there is no
  minute-by-minute sampling while one file stays focused, because the host
  delivers scheduled wakeups only to an instance it recreated after
  reclaiming the idle one (see *Across reclaims* above). A stretch with no
  heartbeat at either end counts nothing at the default settings; with
  `idleMinutes` above the host's reclaim timeout, a scheduled wakeup settles
  its first idle-threshold-worth, once.
- **UTC days.** The host exposes no timezone, so "today" and the day buckets
  are UTC calendar days.
- **Rollover.** A stretch that crosses midnight is credited to the day it
  ends in.
- **Files.** At most 100 files are kept per day (the largest; the dashboard
  shows the top 10), and day buckets older than 90 days are pruned.

## Development

```sh
cargo test                 # logic, against the SDK's mock host
openidea-plugin build      # → dist/plugin.wasm + the install-time checks
```

The plugin depends on the
[`openidea-plugin` SDK](https://github.com/l0ng-ai/openidea/tree/main/sdk/rust/openidea-plugin)
(pinned to tag `v0.1.46`) and builds for `wasm32-wasip2`. The unit tests cover
the heartbeat/idle state machine, day-bucket aggregation, pruning, glob
matching, the dashboard table, the commands and the capability-denied paths.

## License

[Apache-2.0](LICENSE).
