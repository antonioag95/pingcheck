# pingcheck

A single-file ICMP liveness probe that reports to an
[Uptime Kuma](https://github.com/louislam/uptime-kuma) push monitor.

It pings one target, keeps a tiny state machine, and pushes `status`, the
latency and the egress public IP on every tick. It is meant to run as a
long-lived systemd service on a host that sits in a different network than
the monitor, so that reachability is measured from a specific vantage point.

## Behaviour

- One ICMP echo request per iteration, with a 1 s timeout.
- The target name is resolved on every iteration and every returned address
  is tried, so DNS changes are picked up without a cache.
- `down` is reported only after **2 consecutive failed probes**, which keeps
  a single dropped echo from flapping the monitor.
- A push is attempted every iteration, including when the probe failed. That
  covers the case where ICMP is filtered but HTTPS still works: without it the
  monitor would stay green on a host that does not answer pings.
- A failed push is retried 3 times and then logged. The process never exits
  because of a network error.
- The egress IP is fetched once at startup and refreshed every hour, and is
  validated with `IpAddr::from_str`.

## Configuration

Environment only, no configuration file is read.

| Variable | Required | Default | Notes |
| --- | --- | --- | --- |
| `PUSH_URL` | yes | - | base URL, e.g. `https://status.example.net/api/push` |
| `PUSH_TOKEN` | yes | - | monitor token, appended to the path |
| `TARGET` | yes | - | hostname or IP address to ping |
| `INTERVAL` | no | `60` | seconds, accepted range `10..=120` |
| `DRY_RUN` | no | `0` | `1` logs the payload instead of sending it |
| `LOG` | no | `info` | `off`, `error`, `warn`, `info`, `debug` |

Invalid configuration makes the process exit with code 2, so systemd shows
the failure immediately instead of restarting a broken service.

Keep `INTERVAL` well below the heartbeat interval configured on the monitor:
with a 120 s heartbeat a 60 s interval gives two chances before the monitor
declares the target down.

## Dry run

Validate the payload without touching the production monitor:

```sh
DRY_RUN=1 LOG=debug INTERVAL=10 \
PUSH_URL=https://status.example.net/api/push \
PUSH_TOKEN=secret \
TARGET=status.example.net \
./target/x86_64-unknown-linux-musl/release/pingcheck
```

Expected output: one line per iteration with the probe result, plus a
`[dry-run] would push to ...` line showing the exact payload.

## Build

The resulting binaries are fully static and do not need CA certificates on
the target: the trust store is compiled in through `webpki-roots`.

One dependency, `ring` (pulled in by rustls), contains C and assembly code,
so a cross C compiler is required for every musl target. That is why the
build goes through `cargo-zigbuild`: Zig provides both the C compiler and
the linker for all three targets.

```sh
rustup target add x86_64-unknown-linux-musl \
                  aarch64-unknown-linux-musl \
                  armv7-unknown-linux-musleabihf

cargo install cargo-zigbuild
sudo apt-get install -y zig      # or: pipx install ziglang

make build-all
```

`make build-all` runs `cargo zigbuild` for the three targets and prints the
`file` output for each artifact. Harmless linker warnings may appear on the
ARM targets; they do not affect the produced binary.

Supported artifacts:

| Target | Typical host |
| --- | --- |
| `x86_64-unknown-linux-musl` | VPS |
| `aarch64-unknown-linux-musl` | Raspberry Pi 4/5, 64-bit OS |
| `armv7-unknown-linux-musleabihf` | Raspberry Pi 2/3, 32-bit OS |

Run `file target/<target>/release/pingcheck` after building: it must report
a statically linked executable.

## Prebuilt binaries

Every tag ships the three static binaries, so the target machine needs
neither a Rust toolchain nor authentication:

```sh
curl -fL -o pingcheck \
  https://github.com/antonioag95/pingcheck/releases/latest/download/pingcheck-linux-x86_64
curl -fL -o SHA256SUMS \
  https://github.com/antonioag95/pingcheck/releases/latest/download/SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
chmod +x pingcheck
```

Replace `x86_64` with `aarch64` (64-bit Raspberry Pi OS) or `armv7`
(32-bit Raspberry Pi OS).

## Install

```sh
sudo install -m 755 target/x86_64-unknown-linux-musl/release/pingcheck /usr/local/bin/
sudo install -m 600 deploy/pingcheck.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now pingcheck
journalctl -u pingcheck -f
```

Create the real unit from the example and put the token in it:

```sh
sed 's/replace-me/your-token/' deploy/pingcheck.service.example \
  | sudo tee deploy/pingcheck.service >/dev/null
sudo install -m 600 deploy/pingcheck.service /etc/systemd/system/
```

`deploy/pingcheck.service` is git-ignored on purpose: the unit contains the
secret. Mode `600` keeps it readable only by root.

The unprivileged ICMP socket is used, so the service needs no capability and
runs under `DynamicUser`. This requires `net.ipv4.ping_group_range` to cover
all groups, which is the default on Debian, Ubuntu and Raspberry Pi OS.

### SD-card hosts

Use `deploy/pingcheck.service.lowwrite.example`: it sets `LOG=warn` and
`LogLevelMax=warning`, so the per-tick line never reaches the journal and
only warnings and errors are written to the card.

## Portability

The code is deliberately portable: no absolute paths, no `/proc` or `/sys`
access, no external commands, and configuration only from environment
variables. There is currently no Windows artifact. Adding one would require:

- `--once` mode, because the Windows Task Scheduler cannot supervise a loop;
- a service wrapper (`NSSM` or the `windows-service` crate);
- log file rotation, since there is no journald;
- elevated execution, because raw ICMP sockets need administrator rights
  on Windows.

## Tests

```sh
cargo test
```

The tests cover the state machine, the token masking and the latency
formatting. Note that the release profile uses `panic = "abort"`, so
`cargo test --release` does not work; plain `cargo test` uses the test
profile and is unaffected.
