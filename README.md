# moodle2todo-omni

Turns the events of a Moodle calendar feed (`.ics`) into **tasks** (`VTODO`) in any CalDAV calendar.

It talks plain CalDAV (HTTP Basic auth, `GET` and conditional `PUT` on `<calendar>/<name>.ics`), so it is not tied to a particular server. It is used with [anytype-caldav](https://github.com/nrydanov/anytype-caldav) and Nextcloud; other servers should work if they meet the [requirements](#requirements), see [server notes](#server-notes).

## Why

Moodle exports assignments and deadlines as calendar events whose start and end are the same moment. In most calendar clients these show up as zero-length events: thin, unlabeled slivers in the week view, and they can't be marked as done.

Subscribing to the feed on the server side doesn't fix this either. Subscribed calendars are read-only and are usually not exposed to other CalDAV clients.

`moodle2todo` solves both problems: it copies every Moodle event into a regular CalDAV calendar as a task with a due date. Any client that syncs with that calendar can display the tasks properly and mark them as completed.

## How it works

On every run the tool:

1. Downloads the Moodle `.ics` feed.
2. Converts each `VEVENT` into a `VTODO`:
   - `DUE` is taken from `DTEND` (or from `DTSTART` if there is no end).
   - `SUMMARY`, `DESCRIPTION`, `CATEGORIES` and `URL` are copied as they are.
3. Writes each task to the target calendar under a deterministic resource name derived from the event `UID` (`<uid>.ics`). Missing tasks are created; existing ones are updated only if the synced fields changed.

Things worth knowing:

- **Completion status is preserved.** Only `SUMMARY`, `DESCRIPTION`, `DUE`, `CATEGORIES` and `URL` are managed by the tool. Fields such as `STATUS`, `COMPLETED` and `PERCENT-COMPLETE`, which clients set when you tick a task off, are never touched.
- **Nothing is deleted.** Events that disappear from the feed, or that you later add to `EXCLUDE_CATEGORIES`, stay in the calendar. A Moodle feed only covers a limited period, so automatic deletion would erase old completed tasks.
- **Use a dedicated calendar.** Create an empty calendar just for these tasks. If you ran another sync tool into the same calendar before, clear it first, because resource names may differ and produce duplicates.
- **Client rewrites.** If a client re-serializes the synced fields in its own way, the next run overwrites them with the Moodle values. This is harmless but can change the task's ETag.
- The tool makes one `GET` per event on every run, which is fine for hundreds of events at a 30-minute interval.

## Requirements

- **A CalDAV server** that:
  - accepts HTTP Basic authentication;
  - has a calendar collection that allows `VTODO` components (some servers create event-only calendars; pick or create a task-capable one);
  - serves `GET` and `PUT` on `<calendar-url>/<name>.ics`, and honors `If-None-Match: *` and `If-Match: <etag>` on `PUT` (every server that follows RFC 4791 does).
- **Your Moodle calendar export URL**: _Calendar → Export calendar → Get calendar URL_. Treat it as a secret, since it contains your auth token.
- **CalDAV credentials**: a login and a password. If your server supports app-specific passwords, use one instead of your main password.
- **The calendar URL** of an empty, dedicated calendar (see [Finding the calendar URL](#finding-the-calendar-url)).
- Docker with Compose, or a Rust toolchain (and systemd, if you run it as a user service).

## Configuration

All settings are environment variables.

| Variable                 | Required | Description                                                                                                                                                         |
| ------------------------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `MOODLE_ICS_URL`         | yes      | Moodle calendar export URL                                                                                                                                          |
| `CALENDAR_URL`           | yes      | Full URL of the CalDAV calendar collection that will hold the tasks (see [Finding the calendar URL](#finding-the-calendar-url))                                     |
| `CALDAV_USER`            | yes      | CalDAV login                                                                                                                                                        |
| `CALDAV_PASSWORD`        | yes      | CalDAV password or app password                                                                                                                                     |
| `CALDAV_RESOURCE_NAMING` | no       | How `<uid>.ics` is built: `sanitized` (default; unsafe characters become `_`) or `calino` (unsafe bytes become `~XX`, as the Calino client does). See [Resource naming](#resource-naming) |
| `EXCLUDE_CATEGORIES`     | no       | Course names to skip, separated by `;`. See [Skipping courses](#skipping-courses)                                                                                    |
| `SYNC_INTERVAL_SECS`     | no       | Repeat the sync every N seconds. If unset, the tool runs once and exits                                                                                             |
| `CALDAV_CA_FILE`         | no       | Path to a PEM root certificate to trust for the CalDAV server (for private CAs)                                                                                     |
| `CALDAV_INSECURE`        | no       | `true` disables certificate verification for the CalDAV connection. Prefer `CALDAV_CA_FILE`                                                                         |

`CALDAV_INSECURE` and `CALDAV_CA_FILE` apply to the connection to the CalDAV server only. Moodle is always contacted with normal certificate verification.

The old names `NC_USER`, `NC_APP_PASSWORD`, `NC_CA_FILE` and `NC_INSECURE` (from when the tool only supported Nextcloud) are still read when the `CALDAV_*` ones are not set.

### Finding the calendar URL

`CALENDAR_URL` is the address of the calendar *collection*, the one that contains the `<name>.ics` resources. Where to get it depends on the server:

- most web UIs and clients show it as the calendar's **CalDAV URL**, **private link** or **sync URL**;
- otherwise look it up in the server's documentation, or ask the server: `PROPFIND` on the principal or calendar home returns the collections.

Check that the URL and credentials work before the first sync:

```bash
curl -u "$CALDAV_USER:$CALDAV_PASSWORD" -X PROPFIND -H "Depth: 0" "$CALENDAR_URL"
```

A `207 Multi-Status` answer means the URL is a valid collection. Use a calendar created only for these tasks.

### Resource naming

Each task is stored as `<name>.ics`, with `<name>` derived from the Moodle event's `UID` (usually something like `123@moodle.example.com`). Servers differ in what names they accept:

| `CALDAV_RESOURCE_NAMING` | `123@moodle.example.com` becomes | Use it when                                                                          |
| ------------------------ | -------------------------------- | ------------------------------------------------------------------------------------ |
| `sanitized` (default)    | `123_moodle.example.com.ics`     | The server accepts any name. Most general-purpose servers do                         |
| `calino`                 | `123~40moodle.example.com.ics`   | The server requires the name to be a reversible encoding of the `UID`, as anytype-caldav does |

If a `PUT` is rejected with `403`, try `calino`.

> **Changing `CALDAV_RESOURCE_NAMING` on a calendar that already has tasks creates duplicates**, because the resource names change. Decide before the first run.

### Skipping courses

Moodle puts the course name into `CATEGORIES` of every event. To stop syncing finished courses, list their names in `EXCLUDE_CATEGORIES`, separated by `;`:

```
EXCLUDE_CATEGORIES="OOP_2025;Backend (spring)"
```

- Names must match exactly (surrounding spaces are ignored), including case. Quote the value if a name has spaces.
- Find the names with `curl -s "$MOODLE_ICS_URL" | grep -a '^CATEGORIES' | sort | uniq -c`.
- Skipped events are counted as `skipped=N` in the summary line.
- This only affects new syncs. Tasks that were already created stay in the calendar; delete them by hand.

## Server notes

Settings that are specific to a server. Anything not listed here follows the generic rules above.

### anytype-caldav

[anytype-caldav](https://github.com/nrydanov/anytype-caldav) serves Anytype tasks over CalDAV under `/dav/`.

```
CALENDAR_URL=http://127.0.0.1:8080/dav/calendars/tasks/
CALDAV_USER=anytype
CALDAV_PASSWORD=...
CALDAV_RESOURCE_NAMING=calino
```

- **Writing must be enabled** on the server (`[caldav] writable = true`, off by default).
- The login is the server's `caldav.username` (`anytype` unless changed) and the password is its `CALDAV_PASSWORD`. With per-person accounts, use a person's login and their `tasks-<key>/` collection instead.
- **`calino` naming is required.** The server rejects a new task whose name is not derived from its `UID` the way Calino does it (`403 resource name must be derived from the UID`).
- Anytype stores only a subset of `VTODO` fields and renders the rest itself, so `DESCRIPTION` and `URL` are not kept as sent. The tool sees a difference on every run and reports those tasks as `updated`; the server detects that nothing changed and keeps the task intact.

### Nextcloud

```
CALENDAR_URL=https://nextcloud.example.com/remote.php/dav/calendars/USER/moodle-tasks/
```

- Create a calendar for the tasks and copy its URL from the calendar menu → _Copy private link_.
- Use an **app password**: _Personal settings → Security → Devices & sessions → Create new app password_.
- The default `sanitized` naming works.

### Other servers

Not tested, but anything that meets the [requirements](#requirements) should work. Start with the defaults and verify with `curl` as described above. If something fails, see [Troubleshooting](#troubleshooting). Reports and notes for more servers are welcome.

## Usage

### Docker Compose

Put the project in a `moodle2todo/` directory next to your `docker-compose.yml` and add a service:

```yaml
moodle2todo:
  build: ./moodle2todo
  restart: unless-stopped
  environment:
    MOODLE_ICS_URL: "https://moodle.example.com/calendar/export_execute.php?userid=123&authtoken=TOKEN&preset_what=all&preset_time=custom"
    CALENDAR_URL: "https://caldav.example.com/path/to/moodle-tasks/"
    CALDAV_USER: "USER"
    CALDAV_PASSWORD: "PASSWORD"
    # CALDAV_RESOURCE_NAMING: "calino"   # if your server requires it
    SYNC_INTERVAL_SECS: "1800"
```

If the container can't resolve your CalDAV server's hostname (for example a name provided by a VPN's DNS), add an `extra_hosts` entry pointing it to the host. Consider moving secrets into a `.env` file.

Start it:

```bash
docker compose up -d --build moodle2todo
docker compose logs -f moodle2todo
```

Each sync prints a summary line:

```
total=42 created=3 updated=1 unchanged=36 skipped=2 errors=0
```

After changing the environment in `docker-compose.yml`, apply it with `docker compose up -d moodle2todo`; a plain `restart` keeps the old values.

### Without Docker

```bash
export MOODLE_ICS_URL=... CALENDAR_URL=... CALDAV_USER=... CALDAV_PASSWORD=...
cargo run --release
```

Without `SYNC_INTERVAL_SECS` this performs a single sync, so it also works well from cron or a systemd timer.

### systemd user service

Good for a machine that also runs your CalDAV server, or any always-on Linux box. Ready-made files are in [`deploy/`](deploy/).

1. Build and install the binary:

   ```bash
   cargo build --release
   install -Dm755 target/release/moodle2todo ~/.local/bin/moodle2todo
   ```

2. Create the config file and fill it in (see below):

   ```bash
   cp deploy/moodle2todo.env.example ~/.config/moodle2todo.env
   chmod 600 ~/.config/moodle2todo.env
   $EDITOR ~/.config/moodle2todo.env
   ```

3. Install the unit and start it:

   ```bash
   cp deploy/moodle2todo.service ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now moodle2todo
   journalctl --user -u moodle2todo -f
   ```

The unit starts after the network is up, restarts on failure and is limited to 64 MB of memory. If your CalDAV server runs on the same machine as a user unit, add it to `After=` in the unit file so that it starts first.

**Filling in `~/.config/moodle2todo.env`.** It is read by systemd's `EnvironmentFile`, not by a shell:

| Line                     | What to put                                                                                                                       |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------- |
| `MOODLE_ICS_URL`         | The export URL from Moodle, pasted as is. `&` needs no quoting; do not add `export`.                                              |
| `CALENDAR_URL`           | The calendar collection of your server, see [Finding the calendar URL](#finding-the-calendar-url).                                |
| `CALDAV_USER`            | The server login.                                                                                                                 |
| `CALDAV_PASSWORD`        | The server password or app password. If you copy it from another service's env file, copy only this line, not its other secrets.  |
| `CALDAV_RESOURCE_NAMING` | Leave out unless your server needs `calino`, see [Resource naming](#resource-naming).                                             |
| `SYNC_INTERVAL_SECS`     | `1800` for every 30 minutes. Without it the service syncs once and exits.                                                         |
| `EXCLUDE_CATEGORIES`     | Optional, see [Skipping courses](#skipping-courses). Quote it: `EXCLUDE_CATEGORIES="A;B C"`.                                      |

After editing the file apply it with `systemctl --user restart moodle2todo`. After updating the binary, install it again and restart. Because the file contains the Moodle token and the password, keep it at mode `600` and out of version control.

### Client setup

Make sure the new calendar is enabled in your CalDAV client and that the client displays tasks. If you previously subscribed to the Moodle feed in that client, remove the subscription to avoid seeing both events and tasks.

## Troubleshooting

Errors are printed per event as `error: <METHOD> <url> -> <status>` and counted in `errors=N`.

| Message                 | Likely cause                                                                                                                    |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `GET ... -> 401`        | Wrong login or password, or the server needs an app password.                                                                   |
| `PUT ... -> 404` or `409` | `CALENDAR_URL` does not point to an existing calendar collection.                                                            |
| `PUT ... -> 403`        | The server rejects the resource name (try `CALDAV_RESOURCE_NAMING=calino`), or the account/calendar is read-only.                |
| `PUT ... -> 405/415`    | The collection does not accept tasks (event-only calendar) or does not accept `text/calendar`.                                   |
| `PUT ... -> 412`        | The task was changed on the server between the tool's `GET` and `PUT`. It is picked up on the next run.                          |
| `total=N`, nothing created, no errors | Every event matches `EXCLUDE_CATEGORIES` (see `skipped=N`), or the Moodle feed is empty for the chosen period.   |
| TLS errors              | A private CA: set `CALDAV_CA_FILE`. Use `CALDAV_INSECURE` only for testing.                                                     |
