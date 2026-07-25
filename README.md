# ekg

A compact, in-place ping monitor. The machine that goes *ping* — without the wall of scrolling text.

```
● 1.1.1.1   12ms   ▁▂▂▃▂▁▂▂▃▂
  avg 14ms · jitter 2ms · loss 0% · up 2h14m
  last outage: 14:32 (36s)
```

ekg pings a host and renders a small status panel that updates **in place** — it never pushes your terminal down a line per ping. Leave it running in a corner pane all day; when your connection drops, it leaves exactly one permanent line per outage:

```
✗ outage 14:32:05 → 14:32:41 (36s)
```

## Features

- **In-place panel** — 2–3 lines, redrawn each tick; adapts to narrow panes (sparkline and stats shrink to fit)
- **Rolling stats** — average latency, jitter, packet loss % over a sliding window
- **Sparkline** — the last ~20 pings at a glance; timeouts show as red `×`
- **Quality color** — green / yellow / red by latency and loss
- **Outage log** — one permanent line per connection drop, with start time and duration; cap or disable with `-m`
- **No sudo** — uses unprivileged ICMP datagram sockets
- **Fast and tiny** — single static binary, four dependencies

## Usage

```bash
ekg                     # ping 1.1.1.1 every second
ekg 8.8.8.8             # another target
ekg my-router.local     # hostnames work
ekg -i 0.5 -w 120       # every 500ms, 120-sample window
ekg -m 5                # stop logging outage lines after 5 (0 = never log)
```

Ctrl-C prints a session summary: duration, sent/received, loss, min/avg/max, outage count.

## Install

```bash
cargo install --path .
```

Prebuilt binaries, Homebrew, and crates.io coming with the first tagged release.

### Linux note

Unprivileged ICMP requires the ping group range sysctl (most distros ship it enabled):

```bash
sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"
```

## License

MIT OR Apache-2.0, at your option.
