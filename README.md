# tpl

Render templated dotfiles using data sourced from the environment, a config file, or CLI arguments. Supports one-shot rendering and live file watching.

## Install

```sh
cargo install --path .
```

## Config file

Default location: `~/.config/tpl/tpl/config.toml` (override with `-c <path>`).

Multiple `-c` flags can be given. The files are merged in order: `tpls` lists are concatenated and top-level keys are merged with later files winning on conflict. When any `-c` is provided the default config path is not used.

```toml
# Each [[tpls]] entry maps a source template to a rendered destination.
# Paths must be absolute or home-relative (~).
[[tpls]]
src = "~/.config/foo/foo.toml.tpl"
dst = "~/.config/foo/foo.toml"

[[tpls]]
src = "~/.config/bar/bar.conf.tpl"
dst = "~/.config/bar/bar.conf"

# Any top-level key outside [[tpls]] becomes available as cfg.<key> in templates.
theme = "dark"
font_size = 14
```

## Template syntax

Templates use [MiniJinja](https://docs.rs/minijinja) (Jinja2-compatible) syntax. All referenced variables must resolve — undefined variables are a hard error.

### Variable interpolation

```
{{ expression }}
```

### Control flow

```
{% if condition %} ... {% endif %}
{% for item in list %} ... {% endfor %}
```

### Comments

```
{# this is a comment #}
```

### Filters

```
{{ value | upper }}
{{ value | default("fallback") }}
{{ value | replace("old", "new") }}
```

See the [MiniJinja filter reference](https://docs.rs/minijinja/latest/minijinja/filters/index.html) for the full list.

---

## Data sources

### `env` — environment variables

Access any environment variable. Keys are **case-insensitive** (always uppercased internally).

```
{{ env.HOME }}
{{ env.XDG_CONFIG_HOME }}
{{ env.my_var }}   {# resolves MY_VAR from the environment #}
```

### `cfg` — config file values

Access any top-level key from `config.toml` that is not `tpls`.

```
{{ cfg.theme }}
{{ cfg.font_size }}
```

Values can be any TOML type (string, integer, boolean, array, table).

### `cli` — command-line arguments

Pass `key=value` pairs after `--` on the command line:

```sh
tpl -- theme=light font_size=16
```

Access them in templates:

```
{{ cli.theme }}
{{ cli.font_size }}
```

### Magic (bare variable names)

Use a variable directly without a prefix and `tpl` will search all three sources in priority order:

**CLI → environment → config**

```
{{ theme }}   {# checks cli.theme, then env.THEME, then cfg.theme #}
```

The first source to have the key wins. This is useful for variables that can come from any source.

---

## Usage

**One-shot** — render all templates once and exit:

```sh
tpl
tpl -c /path/to/config.toml
tpl -c base.toml -c override.toml
tpl -- key=value key2=value2
```

**Watch mode** — re-render whenever a source template or the config file changes:

```sh
tpl watch
tpl watch -c base.toml -c override.toml
tpl watch --debounce 1s    # default: 500ms
```

**Verbosity:**

```sh
tpl -v     # info
tpl -vv    # debug (also enables deadlock detection)
```
