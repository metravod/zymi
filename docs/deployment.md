# Daemon Deployment

## One-liner with `--daemon`

```bash
curl -fsSL https://raw.githubusercontent.com/metravod/zymi/master/install.sh | sudo bash -s -- --daemon
```

Or from a cloned source tree:

```bash
sudo ./install.sh --daemon
```

## What gets created

### System user

- User `zymi` / group `zymi` (nologin, no home directory)
- Added to `docker` group (if Docker installed) — for container management
- Added to `adm` / `systemd-journal` groups — for log and journal access

### Directories

| Path | Permissions | Contents |
|------|-------------|----------|
| `/opt/zymi` | 750 | Working directory |
| `/opt/zymi/memory` | 700 | Config, schedules, audit trail, baselines |
| `/opt/zymi/.env` | 600 | API keys and secrets |

### Sudoers (`/etc/sudoers.d/zymi`)

NOPASSWD commands — human approves via Telegram:

| Command | Purpose |
|---------|---------|
| `crontab -l` | Cron inspection |
| `smartctl -a/-H/--scan` | Disk S.M.A.R.T health |
| `dmesg` | Kernel messages (OOM, hardware errors) |
| `apt list --upgradable` / `yum check-update` | Security updates check |
| `apt update/install/upgrade/remove` | Package management |
| `systemctl start/stop/restart/enable/disable/status` | Service management |

Journal access comes from group membership (`adm`/`systemd-journal`), not sudoers.
Destructive commands go through the policy engine first — the agent requests approval in Telegram before execution.

### systemd unit (`/etc/systemd/system/zymi.service`)

```
ProtectSystem=strict    ReadWritePaths=/opt/zymi
ProtectHome=true        PrivateTmp=true
ProtectClock=true       ProtectHostname=true
ProtectKernelTunables=true  ProtectKernelModules=true
ProtectControlGroups=true   RestrictNamespaces=true
RestrictRealtime=true   LockPersonality=true
SystemCallArchitectures=native
ProcSubset=all          MemoryMax=512M
```

`NoNewPrivileges` is intentionally off — some agent commands may use sudo.
`ProtectKernelLogs` is intentionally off — `dmesg` needs `CAP_SYSLOG`.

## Post-install

```bash
sudo nano /opt/zymi/.env              # configure API keys
sudo systemctl start zymi             # start
sudo journalctl -u zymi -f            # logs
sudo -u zymi zymi login --remote      # ChatGPT OAuth (headless)
```
