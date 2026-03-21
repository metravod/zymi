# Security

## Policy engine

All shell commands pass through the policy engine before execution. Configured via `memory/policy.json`:

```json
{
  "enabled": true,
  "allow": ["ls *", "cat *", "docker ps *", "docker logs *"],
  "deny": ["rm -rf *"],
  "require_approval": ["docker stop *", "docker rm *", "systemctl restart *"]
}
```

Evaluation order: **deny > require_approval > allow > default (require_approval)**.

Commands not matching any rule require human approval via Telegram (or CLI inline prompt).

## Hardcoded blocks

These are always denied regardless of policy configuration:

| Category | Blocked patterns |
|----------|-----------------|
| **Filesystem** | `rm -rf /`, `mkfs.*`, `dd if=/dev/zero of=/dev/sd*`, `chmod -R 777 /`, `chown -R` |
| **Shell** | Fork bombs |
| **Docker: flags** | `--privileged`, `--device`, `--pid=host`, `--cap-add=ALL`, `--cap-add=SYS_ADMIN`, `apparmor:unconfined` |
| **Docker: mounts** | Bind mounts from `/`, `/etc`, `/root`, `/var/run/docker.sock`, `/run/docker.sock`, `/proc`, `/sys`, `/dev`, `/boot` (and subpaths) |
| **Namespace** | `nsenter -t 1` (host namespace escape) |

Docker group membership is required for container management. Privilege escalation vectors are blocked at the policy engine level. The engine parses mount source paths from `-v`/`--volume`/`--mount` flags and normalizes quotes before checking.

**Limitation:** the policy engine operates on the raw command string before `sh -c` interprets it. Shell variable expansion and subshells can theoretically bypass string checks. Defense-in-depth: the default policy is `require_approval`, so a human sees the command before execution.

## Audit log

Every tool call is logged to `memory/audit.jsonl` (append-only JSONL):

- Event types: `ToolCall`, `ShellCommand`, `ApprovalRequest`, `AgentStart`, `AgentStop`
- Each entry includes arguments preview, result preview, and timestamp
- Non-blocking async writer via mpsc channel
