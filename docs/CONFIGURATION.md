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
  cluster_name: "k3dev"        # namespaces everything: container ({name}-server),
                               # network ({name}-net), volumes, kubelet root, cgroup
                               # root, snapshots, /etc/hosts marker, kube context
  cluster_index: null          # optional; pins the index used to derive pod/service
                               # CIDRs. Omit and k3dev allocates + persists one in
                               # ~/.k3dev/clusters.json
  domain: "local.k8s.dev"      # default domain for ingresses; must be unique per cluster
  k3s_version: "latest"        # k3s image tag; pin e.g. "v1.35.2-k3s1" for a fixed version
  k3s_image_repo: "ghcr.io/daylioti/k3dev-k3s"  # image repo; tag is k3s_version. See note below.
  api_port: 6443               # published directly; must be unique per cluster
  http_port: 80                # host port the shared router listens on
  https_port: 443
  additional_ports:            # extra host:container port mappings
    - "2345:2345"
    - "8080:8080"

  router:                      # shared front router (see "Running several clusters")
    enabled: true
    image: "traefik:v3.3"

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
    MY_VAR: "value"

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

  on_snapshot_created:         # after a snapshot image is committed
    - name: "Notify"
      command: "notify-send 'k3dev snapshot ready'"
      continue_on_error: true
```

Every hook is run with `KUBECONFIG` pointing at a standalone kubeconfig pinned
to the cluster the hook fired for, and `K3DEV_CONTEXT` set to that cluster's
context name. A bare `kubectl` in a hook therefore talks to the right cluster
without depending on your global `current-context` — which k3dev deliberately
never changes, and which may still point at a context left behind by a deleted
cluster. Setting `KUBECONFIG` yourself in `hooks.env` overrides this and gives
up that guarantee.

`on_snapshot_created` fires only when a snapshot is actually written — after the
shallow snapshot taken right after a fresh cluster comes up, and after the deep
snapshot taken once Traefik and the `on_services_deployed` hooks have finished.
Starting from an existing snapshot does not fire it. A failing hook here is
reported but never fails cluster startup, since the cluster is already running.

## K3s image (`k3s_image_repo`)

The cluster image is `{k3s_image_repo}:{k3s_version}`. The default repo,
`ghcr.io/daylioti/k3dev-k3s`, is a k3dev-published rebuild of `rancher/k3s` with the
`socat` and `k3dev-agent` helpers baked in, so a fresh cluster comes up without
runtime binary injection. Tags mirror the upstream `rancher/k3s` tags exactly
(e.g. `v1.36.4-k3s1`), and images are published for `linux/amd64` and `linux/arm64`.

`k3s_version` defaults to `latest`, which tracks the newest k3s release published to
the repo. Note that Docker only resolves a tag when the image is missing locally, so
an already-pulled `latest` keeps being reused until you `docker rmi` it. Snapshots stay
valid too, since the snapshot hash is over the tag string, not the resolved digest — a
kept snapshot restores the old k3s build even after the image is gone. Moving to a newer
`latest` therefore needs both: remove the cached image *and* drop the snapshots
(`k3dev delete-snapshots`, or `k3dev destroy --all`). Pin a version tag when you need a
specific k3s release.

Set `k3s_image_repo: "rancher/k3s"` to use the upstream image instead; k3dev then
injects the helpers into the running container at startup (the previous behavior).
Only the tags published to the chosen repo are available — the k3dev image is built
for the latest patch of the newest four k3s minor lines.

## Command target types

- **`host`** — runs in your local shell; use `workdir` to set the directory. Like hooks, it gets `KUBECONFIG` (the cluster's pinned kubeconfig) and `K3DEV_CONTEXT`, so a bare `kubectl` reaches this cluster whatever your `current-context` is. The same applies to host-target info blocks and `visible:` checks.
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

## Running several clusters at once

Several k3dev clusters can be up and serving simultaneously on the same host
Docker daemon, so pods keep running images built with plain `docker build`.

Give each cluster its own config file and start them with `-c`:

```bash
k3dev -c ./projecta.yml start
k3dev -c ./projectb.yml start
```

Each config must differ in:

| Field | Why |
|---|---|
| `cluster_name` | Namespaces the container, network, volumes, kubelet root-dir, cgroup root, snapshot images, `/etc/hosts` marker and kubeconfig context. |
| `domain` | Host header / SNI is the only thing the shared router can route on, so two clusters must never claim the same domain. |
| `api_port` | Published directly on the host; it can be neither Host- nor SNI-routed. |
| `additional_ports` | Raw host publishes — they collide like any other port. |

`http_port` / `https_port` are the **host** ports the shared `k3dev-router`
container listens on, and are normally 80/443 for every cluster. The in-cluster
Traefik NodePorts stay 80/443 regardless.

### The shared router

With `infrastructure.router.enabled: true` (the default) a single
`k3dev-router` container owns host `:80`/`:443`, attaches to every cluster's
Docker network, and forwards by Host header (HTTP) or SNI (HTTPS, with TLS
passthrough so each cluster's own Traefik still terminates TLS against the
`~/.k3dev/ca` chain). Cluster containers publish no HTTP ports of their own.
This is what lets every cluster keep clean URLs — `https://app.projecta.dev`
with no port suffix.

Set `router.enabled: false` to go back to publishing `http_port`/`https_port`
straight from the cluster container. Only one cluster can then own 80/443, and
the rest need distinct ports and lose clean URLs.

### Networking

Each cluster gets a persisted index (`~/.k3dev/clusters.json`, or pin it with
`cluster_index`) that derives `--cluster-cidr=10.{42+2i}.0.0/16`,
`--service-cidr=10.{43+2i}.0.0/16` and a matching `--cluster-dns`. Index 0
reproduces the k3s defaults exactly. Pod networking is namespaced inside each
container, so overlapping ranges would not break anything, but distinct ones
keep diagnostics honest and leave room for host routes.

### Switching cluster in the TUI

Press `c` (or click the `[name ▾]` badge at the top left, or pick
"Switch Cluster" from the command palette) to switch the running TUI to another
cluster. The overlay lists every cluster, its domain, its API port and whether
it is currently up, filtered as you type.

A cluster is defined by its whole config file — menu, hooks, info blocks,
keybindings, theme — not just a kube context, so switching reopens the TUI
against the target's config rather than repointing the current one. The
cluster → config-file map is built automatically: every config the TUI opens is
remembered in `~/.k3dev/configs.json`. A cluster that is running but was never
opened in the TUI is listed as `no config` and cannot be switched to until you
open it once with `k3dev -c <file>`.

Deleting `~/.k3dev/configs.json` only empties the switcher list; it has no
effect on the clusters themselves.

### Kubeconfig

`~/.kube/config` is **merged**, never overwritten: each cluster contributes a
cluster/user/context all named after `cluster_name`, and any unrelated contexts
are left alone. `kubectl config get-contexts` lists them all;
`kubectl --context projecta ...` targets one.

### How clusters stay out of each other's way

Every cluster runs k3s with `--docker`, so its kubelet drives its own embedded
cri-dockerd against the **shared host Docker daemon** — which is what lets pods
run images built with a plain `docker build`. Left alone, each kubelet would
enumerate the other cluster's pod containers, find their pod UIDs unknown, and
garbage-collect them, including running sandboxes.

k3dev inserts `k3dev-criproxy` between cri-dockerd and dockerd. It stamps a
`k3dev.cluster` label on every container the cluster creates and filters
container listings by that label, so each kubelet only ever sees its own pods.
The binary is baked into the `ghcr.io/daylioti/k3dev-k3s` image and uploaded at
startup otherwise; nothing needs configuring.

Beyond the CRI, each cluster gets its own Docker volumes, kubelet root-dir
(kubelet's manager-state files are flat and unkeyed), and kubelet cgroup root
(`/kubepods-{cluster_name}` — kubelets reconcile away pod cgroups they don't
recognize).

### Known limitations

- A TCP `DOCKER_HOST` (Colima/OrbStack in TCP mode) bypasses the unix-socket
  proxy, so that setup supports one cluster at a time.
- Whichever cluster starts second retunes host-wide `nf_conntrack_max`. k3dev
  sets no conntrack values, so all clusters use identical defaults and this is a
  non-event unless you override them per cluster.
- Image garbage collection is disabled (`image-gc-high-threshold=100`): one
  kubelet must not remove an image another cluster is using. Image lifecycle is
  the host's job — use `docker rmi`.
- There is no cross-cluster pod-to-pod routing.

### Migration from a single-cluster install

Volume and kubelet-root names are now per-cluster, which orphans state written
by older versions. Once, before upgrading:

```bash
k3dev delete                       # or: docker volume rm k3s-rancher-data k3s-local-pv-data
docker rmi $(docker images -q --filter 'reference=k3dev-snapshot-*')
```

The first start after upgrading rebuilds the snapshot. Snapshot images written
by older versions carry no `k3dev.cluster` label, so the now per-cluster
snapshot cleanup (including `destroy --all`) never matches them — hence the
one-off `docker rmi`. Stale unbracketed `# k3dev-ingress` lines in
`/etc/hosts` are cleaned up automatically on the next hosts update.
