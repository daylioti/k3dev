# Configuration Reference

A complete, annotated example of every config section. Every field below is optional — omit anything you don't need and defaults apply.

## File lookup

k3dev loads the first file it finds, in order:

1. `--config <path>` (CLI flag, if given)
2. `./k3dev.yml`
3. `~/.config/k3dev/config.yml`
4. `/etc/k3dev/config.yml`

If none exist, built-in defaults are used. Format is YAML.

## Full example

```yaml
# ---- Kubernetes client -----------------------------------------------------
cluster:
  kubeconfig: ""               # path to kubeconfig; empty = ~/.kube/config
  context: ""                  # context name;       empty = current-context

# ---- K3s infrastructure (the cluster this tool manages) --------------------
infrastructure:
  cluster_name: "k3dev"        # used for container ({name}-server) + network ({name}-net)
  domain: "local.k8s.dev"      # default domain for ingresses
  k3s_version: "v1.35.2-k3s1"  # k3s image tag
  api_port: 6443
  http_port: 80
  https_port: 443
  additional_ports:            # extra host:container port mappings
    - "2345:2345"
    - "8080:8080"

  speedup:                     # snapshot-based fast startup (see note below)
    use_snapshot: true         # first start ~30-60s (creates snapshot); later ~5-10s
    snapshot_auto_cleanup: true  # delete old snapshots when config changes

# ---- UI --------------------------------------------------------------------
ui:
  menu_width: "auto"           # "auto" | percentage e.g. "30%" | fixed int e.g. 40

theme: fallout                 # fallout | cyberpunk | nord

# ---- Logging ---------------------------------------------------------------
logging:
  enabled: true
  file: "/tmp/k3dev-{cluster_name}.log"   # {cluster_name} is substituted at runtime
  level: "info"                # trace | debug | info | warn | error

# ---- Placeholders ----------------------------------------------------------
# Reusable @name values — expanded at load time inside commands/info_blocks.
placeholders:
  ns: "default"
  app_selector: "app.kubernetes.io/name=myapp"

# ---- Custom commands (menu tree) -------------------------------------------
commands:
  - name: "App"
    icon: "web"                # free-form string; no enum
    commands:

      # Kubernetes target (default when `type:` is omitted)
      - name: "Shell"
        description: "Open /bin/sh in the app pod"  # shown in command palette
        exec:
          target:
            type: kubernetes   # optional — kubernetes is the implicit default
            namespace: "@ns"
            selector: "@app_selector"
            container: ""      # optional; empty = first container
            pod_name: ""       # optional; overrides selector if set
          cmd: "/bin/sh"

      # Host target — runs on your machine
      - name: "Git Status"
        exec:
          target: { type: host }
          workdir: "."
          cmd: "git status"

      # Docker target — `docker exec` into a container on the host daemon
      - name: "K3s Processes"
        exec:
          target: { type: docker, container: "k3dev-server" }
          cmd: "ps -ef"

      # Interactive input — @name tokens in `cmd` get filled by user prompts
      - name: "Run drush"
        exec:
          target: { type: kubernetes, namespace: "@ns", selector: "@app_selector" }
          cmd: "drush @command"
          input:
            command: "Enter drush command:"      # bare string = text prompt

      # Richer input forms: text / select / multi-select
      - name: "Deploy with options"
        exec:
          target: { type: kubernetes, namespace: "@ns", selector: "@app_selector" }
          cmd: "deploy.sh --env @env --features @features --note @msg"
          input:
            env:
              type: select
              prompt: "Pick environment:"
              options: [dev, staging, prod]
              default: staging                   # must match one option
            features:
              type: multi-select
              prompt: "Pick features (Space to toggle):"
              options: [auth, logging, metrics]
              default: [auth]                    # values pre-checked
              required: true                     # must select ≥1
            msg:
              type: text
              prompt: "Note:"
              default: "manual"                  # pre-fills the field
              required: true                     # must be non-empty

      # Hide entry unless a check passes (see "Visibility" below)
      - name: "Mailhog UI"
        visible: { type: pod, namespace: "@ns", selector: "app=mailhog" }
        exec:
          target: { type: host }
          cmd: "xdg-open http://mailhog.local"

# ---- Info blocks (sidebar widgets) -----------------------------------------
# Each block runs its `exec` on its own interval and shows the output.
info_blocks:
  - name: "Pods"
    icon: "box"
    exec:
      target: { type: host }
      cmd: "kubectl get pods -A --no-headers | wc -l"
    interval: "10s"            # duration; min 1s; formats: Nms | Ns | Nm | Nh
    max_lines: 5               # keep only last N lines of output (applied first)
    max_length: 200            # UTF-8 safe char cap (applied after max_lines)
    visible: "test -f ~/.kube/config"   # shorthand string → host shell check

# ---- Keybindings -----------------------------------------------------------
# Full list of remappable actions + key-format rules: docs/KEYBINDINGS.md
keybindings:
  quit: "Ctrl+q"
  refresh: "F5"
  command_palette: "Ctrl+p"
  custom:
    "Ctrl+d": "App/Shell"      # value = "Group Name/Command Name"

# ---- Lifecycle hooks -------------------------------------------------------
hooks:
  env:                         # env vars exported to every hook command
    KUBECONFIG: "~/.kube/config"

  on_cluster_available:        # after k3s API responds
    - name: "Wait for nodes"
      command: "kubectl wait --for=condition=ready node --all --timeout=60s"
      workdir: "~"             # supports ~ expansion; default ~
      timeout: 120             # seconds; default 300
      continue_on_error: false # default false
      env:                     # per-hook overrides; merged on top of hooks.env
        EXTRA: "1"

  on_services_deployed:        # after Traefik is deployed
    - name: "Install app chart"
      command: "helm upgrade --install myapp ./charts/myapp"
      workdir: "~/projects/myapp"
      continue_on_error: true
```

## Command target types

- **`host`** — runs in your local shell; use `workdir` to set the directory.
- **`docker`** — `docker exec` into a running container on the host daemon; requires `container`.
- **`kubernetes`** — `kubectl exec` style; pod is located by `selector` OR `pod_name` (one required). Optional `namespace` (defaults to current) and `container` (defaults to first). This is the implicit default when `type:` is omitted.

## Placeholders and @name

Any `@name` token inside a command's `name`, `workdir`, `cmd`, or `target.*` string is replaced at load time with the value from the top-level `placeholders:` map. Tokens from `input:` prompts are filled at execution time instead, and use the same `@name` form inside `cmd`.

## Input prompts (`input:`)

Each entry under `input:` defines one prompt, keyed by the `@name` placeholder it fills. Two YAML shapes are accepted:

- **Shorthand** — bare string is a plain text prompt: `command: "Enter command:"`.
- **Detailed** — map with `type:` of `text`, `select`, or `multi-select`.

| `type:`        | Fields                                            | Substituted value                                        |
| -------------- | ------------------------------------------------- | -------------------------------------------------------- |
| `text`         | `prompt`, `default?`, `required?` (default false) | The text the user typed                                  |
| `select`       | `prompt`, `options`, `default?`                   | The selected option (always exactly one)                 |
| `multi-select` | `prompt`, `options`, `default?`, `required?`      | Selected options joined by a single space (e.g. `a c`)   |

Form keys: `Tab`/`Shift+Tab` move between fields, `Up`/`Down` move within a select / multi-select, `Space` toggles in a multi-select, `Enter` on **Submit** confirms (with required-field validation), `Esc` cancels.

## Visibility (`visible:`)

Hides a command or info block until a check returns true, re-evaluated on `interval` (default `5s`). Supported shapes:

```yaml
visible: "test -f /etc/hosts"                               # shorthand — host shell, exit 0 = visible
visible: { type: pod, namespace: default, selector: app=x } # ≥1 matching pod exists
visible: { type: container, container: k3dev-server }       # docker container exists
visible: { type: exec, target: {...}, cmd: "..." }          # full ExecConfig; exit 0 = visible
visible: { type: pod, ..., interval: "10s" }                # override re-check cadence
```

## Links

- Keybindings reference & key-format rules — [docs/KEYBINDINGS.md](KEYBINDINGS.md)
- CLI flags and headless subcommands — [docs/CLI.md](CLI.md)
- Starter example config — [configs/k3dev.example.yml](../configs/k3dev.example.yml)
