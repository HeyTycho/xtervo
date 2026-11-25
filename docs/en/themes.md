# Themes

Themes recolor the terminal's 16 ANSI colors and the surrounding interface: tab bar, menus, Settings, and the find bar. xtervo ships 12 built-in themes and can load palette files from several other terminals.

## Built-in themes

Select a theme in the Settings panel (⚙). Changing a built-in theme updates the terminal and the interface together.

| ID | Name | Depth |
|---|---|---|
| `yaru-dark` / `yaru-light` | Yaru (Ubuntu/GNOME system look) | Dark / Light |
| `tokyo-night` | Tokyo Night | Dark |
| `catppuccin-mocha` / `-macchiato` / `-frappe` / `-latte` | Catppuccin 4 variants | Dark ×3 / Light |
| `dracula` | Dracula | Dark |
| `nord` | Nord | Dark |
| `gruvbox-dark` | Gruvbox | Dark |
| `one-dark` | One Dark | Dark |
| `solarized-light` | Solarized | Light |

The default is `theme = "auto"`: xtervo follows the system light/dark preference, using **Yaru Dark** for dark and **Yaru Light** for light. `auto_dark` and `auto_light` can point to any built-in or custom theme.

```toml
# ~/.config/xtervo/config.toml
theme = "auto"
auto_dark = "custom:my-dark"    # optional; omitted = Yaru Dark
auto_light = "custom:my-light"  # optional; when omitted, prefer auto_dark's pair, then Yaru Light
```

## Custom theme directory

Put theme files in `~/.config/xtervo/themes/`. The `XTERVO_THEMES_DIR` environment variable can point to a different directory. Restart the application to load changes.

- **One file is one theme.** The theme ID is generated from the file name as `custom:<slug>`, where the slug is lowercase and spaces or symbols are folded into `-`.
- For example, `Dracula.itermcolors` becomes `theme = "custom:dracula"`.
- The directory is scanned at startup. README files, screenshots, and hidden files are ignored.
- A file that fails to parse produces one `[theme] <path>: <reason>` line and does not affect other themes or startup.
- If two files produce the same ID, the file that sorts first is kept.

### Supported formats

These formats can be used directly; no conversion or hand-written config is required.

| Extension | Source / format | Notes |
|---|---|---|
| `.itermcolors` | iTerm2 palette (XML plist) | Most common; iTerm2-Color-Schemes has 200+ palettes |
| `.json` | Windows Terminal `schemes` | A single scheme object or a `schemes` array (the first entry is used) |
| `.conf` / `.config` / `.kitty` | kitty palette | `foreground` / `background` / `cursor` / `color0..15` |
| `.Xresources` / `.txt` / no extension | Xresources | `*.foreground` / `*.background` / `*.color0..15`; Tabby community palettes use the same format |
| `.toml` | Alacritty TOML | With `[colors.primary]` / `[colors.normal]` / `[colors.bright]` |
| `.yaml` / `.yml` | Simple palette YAML | `name` / `foreground` / `background` / `cursor` / a 16-item `colors:` list (Catppuccin and Rosé Pine Tabby snippets) |
| `.toml` | Full xtervo theme | Precise control over 11 interface colors plus a `pair` light/dark link |

Imported formats carry only the terminal's 16 colors plus foreground, background, and cursor. **Interface chrome colors are derived automatically** from those values: background layers, borders, a blue/cyan accent, and a red danger color. Importing a palette therefore does not leave the terminal recolored while the tab bar stays on the old theme.

## Downloading ready-made themes

### iTerm2-Color-Schemes (recommended; widest format coverage)

<https://github.com/mbadolato/iTerm2-Color-Schemes>

```bash
mkdir -p ~/.config/xtervo/themes

# single iTerm2 theme
curl -L -o ~/.config/xtervo/themes/Dracula.itermcolors \
  https://raw.githubusercontent.com/mbadolato/iTerm2-Color-Schemes/master/schemes/Dracula.itermcolors

# batch: clone the repository and copy the kitty palettes (one .conf per theme)
git clone --depth 1 https://github.com/mbadolato/iTerm2-Color-Schemes /tmp/iterm2-themes
cp /tmp/iterm2-themes/kitty/*.conf ~/.config/xtervo/themes/
# you can also copy windowsterminal/*.json, xresources/*, alacritty/*.toml
```

### Catppuccin / Rosé Pine (Tabby palette snippets)

These repositories provide Tabby YAML snippets with `name` + `foreground` + `background` + `cursor` + `colors`. Put a single `.yaml` or `.yml` file in the theme directory; do not put the entire Tabby `config.yaml` there.

```bash
curl -L -o ~/.config/xtervo/themes/catppuccin-mocha.yaml \
  https://raw.githubusercontent.com/catppuccin/tabby/main/themes/catppuccin-mocha.yaml
```

- <https://github.com/catppuccin/tabby>
- <https://github.com/rose-pine/tabby>

### Gogh and other theme sites

Gogh's install script is not a theme file. Take an individual `.itermcolors`, `.conf`, or `.Xresources` file from its repository and place it in the theme directory. <https://github.com/Gogh-Co/Gogh>

## Full xtervo theme format

Hand-write a full theme only when you need precise control over tab bar and menu colors, or when you want to declare a light/dark `pair`.

```toml
schema = 1
name = "My Dark"          # display name in Settings
variant = "dark"          # dark | light
pair = "my-light"         # optional: my-light.toml in the same directory; must be the opposite variant and point back

foreground = "#cdd6f4"
background = "#1e1e2e"
cursor = "#f5e0dc"
# optional: bright_foreground / dim_foreground
ansi = [
  "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
  "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af",
  "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
]

[ui]
bg = "#1e1e2e"
surface = "#181825"
raise = "#313244"
hover = "#45475a"
border = "#585b70"
text = "#cdd6f4"
subtext = "#a6adc8"
muted = "#6c7086"
accent = "#89b4fa"
accent-text = "#11111b"
danger = "#f38ba8"
```

Validation:

- `schema` must be `1`.
- `ansi` must contain exactly 16 entries.
- Every color must be in `#RRGGBB` form.
- All 11 keys under `[ui]` are required: `bg`, `surface`, `raise`, `hover`, `border`, `text`, `subtext`, `muted`, `accent`, `accent-text`, `danger`.
- Unknown fields are rejected, so a typo cannot silently have no effect.
- A `pair` must name a file in the same directory with the opposite `variant`, and the two files must point to each other. An invalid `pair` only clears the pairing; the theme remains usable.

## Per-profile overrides

A profile can pin its own theme in `~/.config/xtervo/profiles.toml`:

```toml
# ~/.config/xtervo/profiles.toml
[[profiles]]
id = 1
name = "Work"
kind = "local"
theme = "catppuccin-mocha"
```

- An explicit profile theme takes precedence over the global `theme`.
- The value can be `auto`, a built-in theme ID, or `custom:<slug>`. `auto` resolves the same way as the global setting.
- A missing or empty `theme` inherits `config.theme`.
- A profile theme affects only the terminal content (renderer background and palette) of panes opened with that profile. The tab bar, menus, Settings, and find bar keep the global interface theme.
- If the ID is unknown, xtervo logs it and falls back to the current global theme. The profile file is not rewritten.
- Appearance fields set by a profile act as per-pane pins. After a profile sets a theme (or font size, cursor style, cursor blink, or scrollback), later global changes do not refresh panes opened with that profile. Clear the profile field and open a new tab to return to inheritance.
- `font_size`, `cursor_shape`, `cursor_blink`, and `scrollback` are the other profile appearance fields and follow the same inheritance rule.

## Restart and troubleshooting

- **Editing, adding, or removing a theme file requires restarting xtervo.** Theme files are scanned at startup and are not hot-reloaded.
- Selecting an already discovered theme in Settings applies immediately. Changes to `theme`, `auto_dark`, and `auto_light` in `config.toml` are also applied while the app is running, but a newly added theme file is not visible until the next start.
- If a theme file has an error, the application prints `[theme] <path>: <reason>` on stderr at startup. Other themes and normal startup are unaffected.
- The default directory is `~/.config/xtervo/themes/`; set `XTERVO_THEMES_DIR` to use another directory.
- A Windows Terminal JSON file with multiple `schemes` entries contributes only the first one. Split the entries into separate files to use all of them.
- YAML import supports only simple palette snippets. To use a full Tabby `config.yaml`, extract its `colorScheme` section first.
- `theme = "auto"` depends on GNOME `gsettings` for system light/dark detection. Non-GNOME desktops fall back to `gtk-theme` / `COLORFGBG` for a one-shot detection, and default to dark when neither is available.
- Imported palettes do not control interface chrome; use the full xtervo format if you need exact tab bar, menu, or Settings colors.
