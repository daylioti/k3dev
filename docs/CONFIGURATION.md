# Configuration Reference

This document provides a complete reference for all k3dev configuration options.

## Configuration File Locations

k3dev searches for configuration files in the following order:

1. `./k3dev.yml` - Current working directory
2. `~/.config/k3dev/config.yml` - User configuration (recommended)
3. `/etc/k3dev/config.yml` - System-wide configuration

The first file found is used. Configuration files are in YAML format.

## Configuration Sections

### cluster

Kubernetes cluster connection settings.

```yaml
cluster:
  kubeconfig: ""   # Path to kubeconfig file
  context: ""      # Kubernetes context to use
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `kubeconfig` | string | `~/.kube/config` | Path to kubeconfig file |
| `context` | string | current context | Kubernetes context to use |

### infrastructure

K3s cluster infrastructure settings.

```yaml
infrastructure:
  domain: "myapp.local"
  k3s_version: "v1.33.4-k3s1"
  container_name: "k3s-server"
  network_name: "k8s-local-net"
  api_port: 6443
  http_port: 80
  https_port: 443
  additional_ports:
    - "2345:2345"
    - "8080:8080"
  auto_update_hosts: false
  deploy_traefik: true
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `domain` | string | - | Domain for local cluster ingress |
| `k3s_version` | string | `v1.33.4-k3s1` | K3s container image version |
| `container_name` | string | `k3s-server` | Docker container name |
| `network_name` | string | `k8s-local-net` | Docker network name |
| `api_port` | integer | `6443` | Kubernetes API port |
| `http_port` | integer | `80` | HTTP ingress port |
| `https_port` | integer | `443` | HTTPS ingress port |
| `additional_ports` | list | `[]` | Additional port mappings (host:container) |
| `auto_update_hosts` | boolean | `false` | Auto-update /etc/hosts with ingress entries |
| `deploy_traefik` | boolean | `true` | Deploy Traefik as ingress controller |

### ui

User interface settings.

```yaml
ui:
  menu_width: "30%"
```

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `menu_width` | string | `auto` | Menu width: `auto`, percentage (`30%`), or fixed pixels |

### theme

UI theme selection.

```yaml
theme: fallout
```

Available themes:
- `fallout` - Green phosphor CRT aesthetic (default)
- `cyberpunk` - Neon purple and cyan
- `nord` - Calm arctic blue-gray palette

### placeholders

Define reusable values that can be referenced in commands using `@placeholder_name` syntax.

```yaml
placeholders:
  default_namespace: "default"
  drupal_selector: "app.kubernetes.io/name=drupal"
  mysql_selector: "app.kubernetes.io/name=mysql"
```

Placeholders are resolved at configuration load time. Use them in command definitions:

```yaml
commands:
  - name: "My Command"
    exec:
      target:
        namespace: "@default_namespace"
        selector: "@drupal_selector"
```

### commands

Define custom commands organized in a hierarchical menu structure.

```yaml
commands:
  - name: "Group Name"
    icon: "web"
    commands:
      - name: "Command Name"
        exec:
          target:
            namespace: "default"
            selector: "app=myapp"
            container: "main"
            pod_name: ""
          workdir: "/app"
          cmd: "my-command"
          input:
            variable_name: "Prompt text:"
```

#### Command Group

| Option | Type | Description |
|--------|------|-------------|
| `name` | string | Display name for the group |
| `icon` | string | Icon identifier (web, database, lightning, wrench) |
| `commands` | list | Nested commands or subgroups |

#### Command Definition

| Option | Type | Description |
|--------|------|-------------|
| `name` | string | Display name for the command |
| `exec` | object | Execution configuration |

#### exec Object

| Option | Type | Description |
|--------|------|-------------|
| `target` | object | Pod targeting configuration |
| `workdir` | string | Working directory inside the container |
| `cmd` | string | Command to execute (supports `@variable` placeholders) |
| `input` | object | Interactive input prompts (key: variable name, value: prompt text) |

#### target Object

| Option | Type | Description |
|--------|------|-------------|
| `namespace` | string | Kubernetes namespace |
| `selector` | string | Label selector to find the pod |
| `container` | string | Container name within the pod (optional) |
| `pod_name` | string | Direct pod name (alternative to selector) |

#### Input Variables

Commands can prompt for user input using the `input` object. Variables are substituted in the `cmd` using `@variable_name` syntax:

```yaml
- name: "Custom Command"
  exec:
    target:
      namespace: "default"
      selector: "app=drupal"
    cmd: "drush @command"
    input:
      command: "Enter drush command:"
```

### keybindings

Customize keyboard shortcuts.

```yaml
keybindings:
  # Built-in actions
  quit: "q"
  help: "?"
  refresh: "r"
  command_palette: ":"
  update_hosts: "H"
  cancel: "Ctrl+c"

  # Navigation
  move_up: "k"
  move_down: "j"
  move_left: "h"
  move_right: "l"
  toggle_focus: "Tab"
  execute: "Enter"

  # Custom command shortcuts
  custom:
    "Ctrl+d": "Drupal Operations/Clear Cache"
    "Ctrl+b": "Database Operations/MySQL/Backup Database"
```

#### Key Format

Keys are specified as strings with optional modifiers:

- Single characters: `q`, `j`, `k`, `?`
- Special keys: `Enter`, `Esc`, `Tab`, `Space`, `Up`, `Down`, `Left`, `Right`
- Function keys: `F1` through `F12`
- With modifiers: `Ctrl+c`, `Alt+x`, `Shift+Tab`
- Multiple modifiers: `Ctrl+Shift+p`

#### Built-in Actions

| Action | Default | Description |
|--------|---------|-------------|
| `quit` | `q`, `Esc` | Exit the application |
| `help` | `?` | Show help overlay |
| `refresh` | `r` | Refresh data |
| `command_palette` | `:` | Open command palette |
| `update_hosts` | `H` | Update /etc/hosts |
| `cancel` | `Ctrl+c` | Cancel current operation |
| `move_up` | `k`, `Up` | Navigate up |
| `move_down` | `j`, `Down` | Navigate down |
| `move_left` | `h`, `Left` | Navigate left / go back |
| `move_right` | `l`, `Right` | Navigate right / enter |
| `toggle_focus` | `Tab` | Toggle focus between panels |
| `execute` | `Enter` | Execute selected command |

#### Custom Command Shortcuts

Map keys directly to commands using the `custom` object. The value is the command path using `/` as separator:

```yaml
custom:
  "Ctrl+d": "Drupal Operations/Clear Cache"
```

### hooks

Execute commands at cluster lifecycle events.

```yaml
hooks:
  env:
    CLUSTER_NAME: "my-cluster"
    KUBECONFIG: "/path/to/kubeconfig"

  on_cluster_available:
    - name: "Setup helm repos"
      command: "helm repo update"
      workdir: "~"
      timeout: 300
      continue_on_error: false

  on_services_deployed:
    - name: "Deploy application"
      command: "helm install myapp ./charts/myapp"
      workdir: "~/projects/myapp"
      timeout: 600
      continue_on_error: true
```

#### env

Environment variables passed to all hook commands.

#### Hook Events

| Event | When Triggered |
|-------|----------------|
| `on_cluster_available` | After K3s container is running and API is accessible |
| `on_services_deployed` | After Traefik is deployed (if enabled) |

#### Hook Definition

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `name` | string | - | Display name for the hook |
| `command` | string | - | Shell command to execute |
| `workdir` | string | `~` | Working directory (supports `~` expansion) |
| `timeout` | integer | `300` | Timeout in seconds |
| `continue_on_error` | boolean | `false` | Continue with next hook if this one fails |

## Complete Example

See `configs/k3dev.example.yml` for a complete configuration example with a Drupal development environment setup.

```yaml
cluster:
  kubeconfig: ""
  context: ""

infrastructure:
  domain: "myapp.local"
  k3s_version: "v1.33.4-k3s1"
  container_name: "k3s-server"
  network_name: "k8s-local-net"
  api_port: 6443
  http_port: 80
  https_port: 443
  additional_ports:
    - "2345:2345"
  auto_update_hosts: false
  deploy_traefik: true

ui:
  menu_width: "30%"

theme: fallout

placeholders:
  default_namespace: "default"
  app_selector: "app.kubernetes.io/name=myapp"

commands:
  - name: "Application"
    icon: "web"
    commands:
      - name: "Shell"
        exec:
          target:
            namespace: "@default_namespace"
            selector: "@app_selector"
          cmd: "/bin/bash"

keybindings:
  custom:
    "Ctrl+s": "Application/Shell"

hooks:
  on_cluster_available:
    - name: "Wait for ready"
      command: "kubectl wait --for=condition=ready node --all --timeout=60s"
      timeout: 120
```
