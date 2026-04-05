# k3dev

A TUI for managing local K3s clusters in Docker.

![License](https://img.shields.io/badge/license-MIT-blue)
![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS-lightgrey)

## Features

- **Cluster Lifecycle** - Start, stop, restart, and delete K3s clusters
- **Fast Startup via Snapshots** - First start creates a snapshot image; subsequent starts take seconds
- **Headless CLI Mode** - Run cluster actions, diagnostics, and pod operations without the TUI
- **Diagnostics & Preflight Checks** - Verify the cluster is healthy or ready to start
- **Image Pull Progress** - Byte-level progress bars for Docker image pulls
- **Pod Operations** - Execute commands inside pods with an interactive terminal
- **Ingress Management** - View endpoints with health checks and `/etc/hosts` integration
- **Custom Commands** - Hierarchical command menus with placeholders and keybind shortcuts
- **Resource Monitoring** - CPU and memory stats for containers and pods
- **Hooks** - Run shell commands on `on_cluster_available` / `on_services_deployed`
- **Docker Passthrough** - `k3dev docker ...` targets the cluster's Docker daemon
- **Themes** - Fallout, Cyberpunk, and Nord
- **Vim-style Navigation** - Customizable keybindings

## Requirements

- Linux or macOS (Docker Desktop)
- Docker (running; Linux needs the `cgroupfs` cgroup driver)
- kubectl (only required for interactive `exec` and `logs --follow`)
- HTTPS certificates are generated automatically (built-in CA)

### Docker Configuration (Linux)

k3dev runs K3s inside Docker containers. On Linux, Docker must be configured to use the `cgroupfs` cgroup driver:

```bash
sudo mkdir -p /etc/docker
sudo tee /etc/docker/daemon.json <<EOF
{
    "exec-opts": ["native.cgroupdriver=cgroupfs"]
}
EOF
sudo systemctl restart docker
```

Verify with:
```bash
docker info | grep -i cgroup
# Should show: Cgroup Driver: cgroupfs
```

On macOS, Docker Desktop handles this automatically — no configuration required.

## Installation

### Quick Install (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/daylioti/k3dev/main/install.sh | bash
```

This automatically detects your architecture and installs to `~/.local/bin`.

### cargo binstall

If you have [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```bash
cargo binstall k3dev
```

### Manual Download

Download the latest binary from [Releases](https://github.com/daylioti/k3dev/releases).

```bash
chmod +x k3dev-linux-x86_64
sudo mv k3dev-linux-x86_64 /usr/local/bin/k3dev
```

### From Source

```bash
git clone https://github.com/daylioti/k3dev.git
cd k3dev
cargo build --release
# Binary: target/release/k3dev
```

## Configuration

Configuration is loaded from (in order):
1. `./k3dev.yml`
2. `~/.config/k3dev/config.yml`
3. `/etc/k3dev/config.yml`

```bash
cp configs/k3dev.example.yml ~/.config/k3dev/config.yml
```

### Example

```yaml
cluster:
  kubeconfig: ""
  context: ""

infrastructure:
  cluster_name: "k3dev"
  domain: "myapp.local"
  k3s_version: "v1.35.2-k3s1"

theme: fallout

placeholders:
  ns: "default"
  app: "app=myapp"

commands:
  - name: "App"
    commands:
      - name: "Shell"
        exec:
          target:
            namespace: "@ns"
            selector: "@app"
          cmd: "/bin/sh"

keybindings:
  quit: "q"
  help: "?"
  refresh: "r"
  command_palette: ":"
  custom:
    "Ctrl+s": "App/Shell"

hooks:
  on_cluster_available:
    - name: "Setup"
      command: "helm repo update"
  on_services_deployed:
    - name: "Deploy"
      command: "helm install myapp ./charts/myapp"
```

## CLI (Headless Mode)

Running `k3dev` with no arguments launches the TUI. Passing a subcommand runs the action headlessly and exits — useful for scripts, CI, and shell aliases.

```bash
# Cluster lifecycle
k3dev start              # Start the cluster
k3dev stop               # Stop the cluster
k3dev restart            # Restart the cluster
k3dev destroy            # Delete the cluster
k3dev info               # Show cluster info
k3dev delete-snapshots   # Delete all snapshot images

# Health
k3dev preflight          # Verify the cluster can start
k3dev diagnostics        # Run full cluster diagnostics

# Networking
k3dev update-hosts       # Sync /etc/hosts with ingress entries

# Pods
k3dev pods [-n NS]                       # List pods
k3dev logs POD [-n NS] [-f] [--tail N]   # View pod logs
k3dev describe POD [-n NS]               # Describe a pod
k3dev exec POD [-n NS] [--cmd /bin/sh]   # Interactive shell into a pod
k3dev delete-pod POD [-n NS]             # Delete a pod
k3dev restart-pod POD [-n NS]            # Delete and let deployment recreate

# Docker passthrough — targets the cluster's Docker daemon
k3dev docker ps
k3dev docker images
```

All subcommands accept `-c, --config <PATH>` to override the config file location.

See [docs/CLI.md](docs/CLI.md) for the full reference.

## Keybindings

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `?` | Help |
| `r` | Refresh |
| `:` | Command palette |
| `j/k` or `↑/↓` | Navigate |
| `h/l` or `←/→` | Back / Enter |
| `Enter` | Execute |
| `Tab` | Switch panel |
| `H` | Update /etc/hosts |

Vim-style number prefixes supported (e.g., `3j`).

## License

MIT
