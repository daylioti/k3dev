# Keybindings Reference

This document provides a quick reference for all k3dev keyboard shortcuts.

## Default Keybindings

### Application Controls

| Key | Action |
|-----|--------|
| `q` | Quit application |
| `Esc` | Quit / Cancel / Close popup |
| `Ctrl+q` | Quit application |
| `Ctrl+c` | Cancel current operation |
| `?` | Show/hide help overlay |
| `r` | Refresh data |
| `:` | Open command palette |
| `H` | Update /etc/hosts with ingress entries |

### Navigation

| Key | Action |
|-----|--------|
| `j` / `Down` | Move down |
| `k` / `Up` | Move up |
| `h` / `Left` | Move left / Go back |
| `l` / `Right` | Move right / Enter submenu |
| `Tab` | Toggle focus between panels |
| `Enter` | Execute selected command |

### Vim-style Number Prefixes

You can prefix navigation keys with numbers for repeated movement:

- `3j` - Move down 3 items
- `5k` - Move up 5 items
- `10j` - Move down 10 items

## Customizing Keybindings

Add a `keybindings` section to your configuration file:

```yaml
keybindings:
  # Remap built-in actions
  quit: "Ctrl+q"
  help: "F1"
  refresh: "F5"
  command_palette: "Ctrl+p"

  # Navigation remaps
  move_up: "w"
  move_down: "s"

  # Custom command shortcuts
  custom:
    "Ctrl+d": "Drupal Operations/Clear Cache"
    "Ctrl+b": "Database/Backup"
```

## Key Format

Keys are specified as strings with optional modifiers.

### Single Keys

```
q, j, k, l, h, ?, r
```

### Special Keys

```
Enter, Return
Esc, Escape
Tab, BackTab
Space
Backspace
Delete, Del
Insert, Ins
Home, End
PageUp, PgUp
PageDown, PgDn
Up, ArrowUp
Down, ArrowDown
Left, ArrowLeft
Right, ArrowRight
F1, F2, ... F12
```

### Modifier Keys

Combine with `+`:

```
Ctrl+c
Alt+x
Shift+Tab
Ctrl+Shift+p
```

Supported modifiers:
- `Ctrl` or `Control`
- `Alt`
- `Shift`

## Remappable Actions

| Config Key | Default | Description |
|------------|---------|-------------|
| `quit` | `q` | Exit the application |
| `help` | `?` | Toggle help overlay |
| `refresh` | `r` | Refresh all data |
| `command_palette` | `:` | Open command palette |
| `update_hosts` | `H` | Update /etc/hosts file |
| `cancel` | `Ctrl+c` | Cancel running operation |
| `move_up` | `k` | Navigate up |
| `move_down` | `j` | Navigate down |
| `move_left` | `h` | Navigate left / back |
| `move_right` | `l` | Navigate right / enter |
| `toggle_focus` | `Tab` | Switch focus between panels |
| `execute` | `Enter` | Execute selected item |

## Custom Command Shortcuts

Map keys directly to commands in your menu:

```yaml
keybindings:
  custom:
    "Ctrl+1": "Drupal Operations/Clear Cache"
    "Ctrl+2": "Drupal Operations/Import Configuration"
    "Ctrl+3": "Database Operations/MySQL/Show Databases"
```

The command path uses `/` as separator and must match the exact command names in your menu configuration.

## Context-Sensitive Keys

Some keys behave differently based on context:

| Context | Key | Behavior |
|---------|-----|----------|
| Normal mode | `Esc` | Quit application |
| Input mode | `Esc` | Cancel input |
| Help overlay | `Esc` | Close help |
| Output popup | `Esc` | Close popup |
| Command palette | `Esc` | Close palette |

## Mouse Support

k3dev supports mouse interaction:

- **Click** on menu items to select
- **Click** on action bar buttons to trigger cluster actions
- **Click** on ingress links (if terminal supports)

Mouse interaction works alongside keyboard navigation.
