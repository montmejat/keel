# 0018: Provisioning over SSH, a systemd service, and a shared token

**Status:** Proposed, 2026-10-01

## Context
A machine became a keel machine by hand: cross-compile `keel`, copy it,
start `keel daemon` in a terminal, open it to the network. Anyone who could
reach a daemon could make it run any program ([0006](0006-coordinator-and-daemons.md),
[0016](0016-packaging-and-deployment.md)).

## Decision
- **`keel provision <host>`, over plain `ssh`** (the user's `~/.ssh/config`
  applies): it asks the host its architecture, user, sudo and systemd, builds
  `keel` for it (static, as in 0016), and copies it to `~/.local/bin/keel`
  over the SSH connection's stdin, unless the host already has that exact
  binary (by SHA-256). Run it again to upgrade; it's idempotent.
- **A systemd service.** With passwordless sudo: a system unit running as
  the user, with `LimitRTPRIO=95` and `LimitMEMLOCK=infinity`, so real-time
  nodes get SCHED_FIFO without touching `/etc/security/limits.d` (user
  services don't go through `pam_limits`). Without sudo: a user unit and
  lingering, without real-time priority, and keel says so. Logs go to the
  journal (`journalctl -u keel`); systemd restarts the daemon if it fails.
- **A shared token.** `keel provision` creates `~/.config/keel/token` (256
  random bits, mode 600) on first use and copies it to every host it
  provisions. Every connection to a daemon (coordinator, deploy, peer
  data, binary upload) starts with its kind byte and the token on a line;
  a daemon with a token refuses connections without it, compared in
  constant time, and says why. A daemon without a token accepts everything,
  as before, and warns.
- `keel provision --remove <host>` stops and removes the service, the binary
  and the token, and keeps the store.

## Consequences
- A machine needs only `sh`, `sha256sum`, systemd and SSH access. Updating
  keel everywhere is `keel provision` per host.
- The provisioned daemon listens on all interfaces (port 7400), which the
  token makes reasonable on a trusted LAN. It authenticates, it doesn't
  encrypt: anyone who can sniff the network can read the token and the
  data. On an untrusted network, keel belongs on a WireGuard VPN, or behind
  SSH tunnels.
- One token per deploying machine: two people provisioning the same host
  from two laptops overwrite each other's. Fine for one person's robots.
- `keel provision` builds keel, so it runs from keel's source tree.
