# Internet Status

A small macOS desktop app that watches for the intermittent breaks where the
local network is fine but nothing on the Internet is reachable. It shows the
current state, charts latency, captures a traceroute *while* an outage is
happening, and appends every outage to a log you can read later.

Built with [`iced`](https://iced.rs) 0.14.

## Running

```sh
cargo run --release              # the app
cargo run --release -- --probe   # one headless probe round, printed
cargo run --release -- --speedtest  # one headless speed test, printed and logged
./make-app.sh                    # bundle "Internet Status.app"
```

`--probe` runs the same checks without opening a window and prints a traceroute
if anything upstream is failing. Useful from a shell or `cron` when the GUI
disagrees with what you think is reachable.

## Menu bar

The app lives in the menu bar as a coloured dot: green online, amber packet
loss, red outage, grey ring paused. Its menu shows the current status and
latency, a summary of today's outages, live traffic, the latest speed test,
and **Check
now**, **Run speed test**, **Pause**, **Show window**, **Open log folder** and
**Quit**.

Closing the window does not quit — monitoring continues from the menu bar, and
**Show window** brings it back. Quit from the menu.

`./make-app.sh` bundles the release binary with `LSUIElement` set, so the app
has no Dock icon and no app-switcher entry, like other menu-bar utilities. Add
the bundle under **System Settings > General > Login Items** to have it start
with the machine.

## How the diagnosis works

Each round probes four things, layered so a failure points at a culprit:

| Target | Probe | What its failure means |
| --- | --- | --- |
| Home router | ICMP echo | Wi-Fi or cabling — the fault is local |
| `1.1.1.1`, `8.8.8.8` | TCP `:443` | Routing is broken upstream of your router |
| `www.google.com` | DNS lookup + TCP `:443` | Name resolution is failing |

The verdicts, in the order they are checked:

- **LAN DOWN** — the router itself did not answer. Nothing upstream matters yet.
- **INTERNET DOWN** — the router answered, nothing beyond it did. The fault is
  at or past your ISP's first hop.
- **DNS DOWN** — public addresses are reachable and a name genuinely failed to
  *resolve*. A hostname that resolved and then timed out is not this; see
  below.
- **PACKET LOSS** — some targets missed replies. Usable, and *not* logged as an
  outage.
- **ONLINE** — everything answered.

An outage is only declared after `fail_threshold` consecutive bad rounds (2 by
default), so a single dropped packet is never logged as an outage. It is only
closed after `recover_threshold` consecutive clean rounds (3 by default), so a
connection that flaps for ten minutes is one outage rather than sixteen. The
recorded end time is the *first* clean round — the moment the connection came
back, not the later moment recovery was confirmed.

The traceroute starts on the **first** bad round, before the threshold is met.
Waiting for the threshold means the trace runs after a short outage is already
over: in four days of logs, 95% of outages lasting 15s or less had captured a
perfectly healthy path, because the trace began after the connection returned.

### "DNS down" means resolution failed, nothing else

A hostname target needs two things to succeed: the name must resolve, and the
connection must complete. Only the first failing is a DNS fault. The probe
records *which* one failed (`probe::Failure`) rather than leaving it to be
guessed from the error text, because the two lead you to opposite places.

This distinction was not free. An earlier version blamed DNS whenever the
hostname target failed, and labelled 34 outages "DNS down" across four days.
Resolution had succeeded in every one of them; the connection had timed out.
The real cause was upstream packet loss, the same as the other 75.

### Why TCP and not ping, for the Internet targets

ICMP echo is not trustworthy as a connectivity signal on many consumer
connections. On the network this was developed against, echoes to public
addresses come back as:

```
36 bytes from 10.0.100.9: Communication prohibited by filter
```

An upstream device drops or rate-limits them, which is indistinguishable from a
real outage. A TCP handshake to port 443 gets through, and it also proves
something more useful: that a connection actually completes end to end.

A *refused* connection counts as reachable. Something sent a packet back, which
is exactly what an outage does not do.

The router is still probed with ICMP: routers answer echoes reliably and expose
no port worth relying on.

### Do not use port 53

Home routers commonly intercept DNS. A TCP connection to *any* address on port
53 will succeed against your own router in about a millisecond, with the
Internet completely down:

```
1.1.1.1:53     1.0 ms      <- the router answering, not Cloudflare
1.1.1.1:443   28.1 ms      <- an actual round trip
```

## Speed tests

Download and upload speed come from Apple's `/usr/bin/networkQuality`, which
ships with macOS 12 and later. Besides throughput it reports
*responsiveness* in round trips per minute while the link is loaded; a low
figure next to high throughput is bufferbloat. On older macOS the speed panel
says so and the menu item does nothing.

A test fills the line in both directions for up to `speedtest_max_secs` and
moves several hundred megabytes on a fast connection (about 800 MB at
450/50 Mbps), so automatic tests are **off by default**. Run one from the
window or the menu bar, or set `speedtest_interval_mins` (minimum 15).
Scheduled tests are skipped while paused or during an outage.

While a test runs, probe rounds are still shown but kept out of outage
detection: a saturated link times probes out exactly the way an outage does.

## Live traffic

Separately from speed tests, the app shows how much the machine is using
right now — `Now on en0: ↓ 12.4 Mbps ↑ 820 kbps` — in the window and the menu
bar. It is read from the kernel's byte counters for the default-route
interface (`netstat -ib`) on every probe round, so it costs no data. It says
nothing about what the line can do: an idle machine reads near zero.

When an outage is declared, the busiest sample from the minute before is
saved with it as `traffic_before`. A line that was saturated just before it
"went down" is more likely congested than dead.

## Files

All three live in `~/Library/Application Support/internet_status/`, and the **Open
log** button reveals the folder in Finder.

- `config.toml` — written with defaults on first run; edit and restart.
- `outages.jsonl` — one JSON object per line.
- `speedtests.jsonl` — one speed test per line: `down_mbps`, `up_mbps`,
  `responsiveness_rpm`, `base_rtt_ms`, `interface`, `bytes_transferred`, and
  `error` when the test failed.

A record is written when the outage is detected, again when the traceroute
finishes, and again when it ends. Later lines win on reload, so an outage that
still has `"ended_at": null` after a restart means the app stopped while it was
still down.

```json
{
  "id": "1788537312886",
  "status": "internet_down",
  "started_at": "2026-09-04T09:55:12.886490-06:00",
  "ended_at": "2026-09-04T09:55:31.004112-06:00",
  "duration_secs": 18,
  "failed_targets": ["Cloudflare (tcp/443 · timed out)"],
  "traceroute": [{ "hop": 1, "host": "192.168.88.1", "latency_ms": 2.512 }],
  "last_responding_hop": 3,
  "traffic_before": { "interface": "en0", "down_mbps": 41.2, "up_mbps": 1.3 }
}
```

`last_responding_hop` is the useful field: the last device on the path that
still answered while the connection was down.

## Configuration

```toml
interval_secs = 5        # seconds between probe rounds
fail_threshold = 2       # consecutive bad rounds before an outage is logged
recover_threshold = 3    # consecutive clean rounds before it is closed
timeout_ms = 2000        # per-probe timeout
traceroute_max_hops = 15
history_len = 240        # probe rounds kept in the latency chart
gateway = ""             # blank auto-detects via `route get default`
speedtest_interval_mins = 0  # automatic speed tests; 0 is off, minimum 15
speedtest_max_secs = 20      # networkQuality -M; bounds one test

[[targets]]
name = "Cloudflare"
host = "1.1.1.1"
kind = "internet"        # gateway | internet | dns
method = "tcp"           # tcp | icmp
port = 443
```

`kind` is the target's role in the diagnosis; `method` is how it is tested. The
gateway target is added automatically and is not listed in the file.

## Layout

| File | Contents |
| --- | --- |
| `src/main.rs` | `iced` application: state, update, view, `--probe` and `--speedtest` |
| `src/probe.rs` | ICMP, TCP and traceroute, plus the output parsers |
| `src/speed.rs` | `networkQuality` runner, parser and speed-test log |
| `src/traffic.rs` | Live throughput from interface byte counters |
| `src/outage.rs` | Verdict classification and the JSONL log |
| `src/config.rs` | Config file and defaults |
| `src/chart.rs` | Latency sparkline (`iced` canvas) |
| `src/tray.rs` | Menu-bar status item and its menu (`tray-icon`) |

Probes shell out to `/sbin/ping` and `/usr/sbin/traceroute`, so the app needs no
raw-socket privileges and no entitlement. Every probe in a round runs
concurrently on a blocking thread pool, and a round is skipped if the previous
one is still running.

The menu-bar item has to be created and updated on the main thread, so it is
built from the first `update` call — by then the application is running and the
status bar will accept it — and `tray_icon`'s process-wide event channel is
drained on a 150 ms tick rather than from a background stream.

## Reading the log

Records are not episodes. A flapping connection produces several records; the
analysis that shaped this tool collapsed 109 records into 21 episodes by
merging anything less than five minutes apart. `recover_threshold` reduces this
but does not eliminate it, so merge before counting.

## Tests

```sh
cargo test
```

The parsers and the classification table are covered. Two tests touch real DNS.
