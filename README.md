# k3dev

A TUI for managing local K3s clusters in Docker.

![License](https://img.shields.io/badge/license-MIT-blue)
![Linux](https://img.shields.io/badge/platform-Linux-lightgrey)

## Features

- **Cluster Lifecycle** - Start, stop, restart, and delete K3s clusters
- **Pod Operations** - Execute commands inside pods with interactive terminal
- **Ingress Management** - View endpoints with health checks and `/etc/hosts` integration
- **Custom Commands** - Hierarchical command menus for common tasks
- **Resource Monitoring** - CPU and memory stats for containers and pods
- **Themes** - Fallout, Cyberpunk, and Nord
- **Vim-style Navigation** - Customizable keybindings

## Requirements

- Linux
- Docker (running)
- kubectl
- mkcert (optional, for HTTPS)

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
  domain: "myapp.local"
  k3s_version: "v1.33.4-k3s1"
  container_name: "k3s-server"
  network_name: "k8s-local-net"
  auto_update_hosts: false
  deploy_traefik: true

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
