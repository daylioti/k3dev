# CLI Reference

Running `k3dev` with no arguments launches the interactive TUI. Passing any subcommand runs the action headlessly, prints colored output to stdout, and exits with a conventional status code (`0` on success, `1` on failure).

## Global Flags

| Flag | Description |
|------|-------------|
| `-c, --config <PATH>` | Override the config file location. Applies to every subcommand. |
| `-h, --help` | Print help |
| `-V, --version` | Print version |

Config resolution (when `--config` is not provided) follows the same order as the TUI: `./k3dev.yml` → `~/.config/k3dev/config.yml` → `/etc/k3dev/config.yml`.

## Cluster Lifecycle

| Command | Description |
|---------|-------------|
| `k3dev start` | Start the cluster (uses a snapshot if available). |
| `k3dev stop` | Stop the running cluster container. |
| `k3dev restart` | Stop then start the cluster. |
| `k3dev destroy` | Delete the cluster container and associated resources. |
| `k3dev info` | Show cluster metadata (name, version, endpoints). |
| `k3dev delete-snapshots` | Remove all snapshot images created by k3dev. |

## Health Checks

| Command | Description |
|---------|-------------|
| `k3dev preflight` | Verify the host is ready to start a cluster (Docker, ports, cgroups, etc.). Non-zero exit on failure. |
| `k3dev diagnostics` | Run the full diagnostics suite against a running cluster. Non-zero exit if any check fails. |

Output is streamed as each check runs, with a pass/fail summary at the end.

## Networking

| Command | Description |
|---------|-------------|
| `k3dev update-hosts` | Sync `/etc/hosts` with ingress entries. Falls back to printing lines if the file is read-only or requires sudo. |

## Pod Operations

| Command | Description |
|---------|-------------|
| `k3dev pods [-n, --namespace NS]` | List pods. Defaults to all namespaces. |
| `k3dev logs POD [-n NS] [--container C] [-t, --tail N] [-f, --follow]` | View pod logs. `--follow` delegates to `kubectl logs -f`. |
| `k3dev describe POD [-n NS]` | Describe a pod (equivalent to `kubectl describe pod`). |
| `k3dev exec POD [-n NS] [--container C] [--cmd /bin/sh]` | Interactive shell/command inside a pod. Uses `kubectl exec -it` for a real TTY. |
| `k3dev delete-pod POD [-n NS]` | Delete a pod. |
| `k3dev restart-pod POD [-n NS]` | Delete a pod and let its controller recreate it. |

Defaults: `--namespace default`, `--tail 100`, `--cmd /bin/sh`.

## Docker Passthrough

| Command | Description |
|---------|-------------|
| `k3dev docker <args>...` | Run `docker` against the cluster's Docker daemon, bypassing any Docker Desktop proxy. All arguments are forwarded verbatim. |

Examples:

```bash
k3dev docker ps
k3dev docker images
k3dev docker logs -f <container>
```

## Examples

```bash
# Start the cluster and tail logs of a pod, in one shell session
k3dev start && k3dev logs my-app -f

# Use an alternate config file for a second cluster
k3dev -c ./ci.k3dev.yml start
k3dev -c ./ci.k3dev.yml diagnostics

# Shell into the first drupal pod in the `web` namespace
k3dev exec drupal-0 -n web --cmd /bin/bash

# Run a preflight check in CI, fail the job if anything is wrong
k3dev preflight || exit 1
```

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | Success |
| `1` | Operation failed, timed out, or a health check reported failures |
