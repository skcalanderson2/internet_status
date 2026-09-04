# Internet Status

A small macOS desktop app that watches for the intermittent breaks where the
local network is fine but nothing on the Internet is reachable. It shows the
current state, charts latency, captures a traceroute *while* an outage is
happening, and appends every outage to a log you can read later.

Built with [`iced`](https://iced.rs) 0.14.

## Running

```sh
cargo run --release          # the app
cargo run --release -- --probe   # one headless probe round, printed
```

`--probe` runs the same checks without opening a window and prints a traceroute
if anything upstream is failing. Useful from a shell or `cron` when the GUI
disagrees with what you think is reachable.

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
- **DNS DOWN** — public addresses are reachable but hostnames do not resolve.
- **PACKET LOSS** — some targets missed replies. Usable, and *not* logged as an
  outage.
- **ONLINE** — everything answered.

An outage is only declared after `fail_threshold` consecutive bad rounds (2 by
default), so a single dropped packet is never logged as an outage.

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

## Files

Both live in `~/Library/Application Support/internet_status/`, and the **Open
log** button reveals the folder in Finder.

- `config.toml` — written with defaults on first run; edit and restart.
- `outages.jsonl` — one JSON object per line.

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
  "last_responding_hop": 3
}
```

`last_responding_hop` is the useful field: the last device on the path that
still answered while the connection was down.

## Configuration

```toml
interval_secs = 5        # seconds between probe rounds
fail_threshold = 2       # consecutive bad rounds before an outage is logged
timeout_ms = 2000        # per-probe timeout
traceroute_max_hops = 15
history_len = 240        # probe rounds kept in the latency chart
gateway = ""             # blank auto-detects via `route get default`

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
| `src/main.rs` | `iced` application: state, update, view, and `--probe` |
| `src/probe.rs` | ICMP, TCP and traceroute, plus the output parsers |
| `src/outage.rs` | Verdict classification and the JSONL log |
| `src/config.rs` | Config file and defaults |
| `src/chart.rs` | Latency sparkline (`iced` canvas) |

Probes shell out to `/sbin/ping` and `/usr/sbin/traceroute`, so the app needs no
raw-socket privileges and no entitlement. Every probe in a round runs
concurrently on a blocking thread pool, and a round is skipped if the previous
one is still running.

## Tests

```sh
cargo test
```

The parsers and the classification table are covered. Two tests touch real DNS.
