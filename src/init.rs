use anyhow::Result;
use clap::ValueEnum;
use std::path::PathBuf;

#[derive(Clone, ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
    Nu,
}

pub fn run(shell: &Shell, starship: bool) -> Result<()> {
    let hook = match shell {
        Shell::Zsh => ZSH_HOOK,
        Shell::Bash => BASH_HOOK,
        Shell::Fish => FISH_HOOK,
        Shell::Nu => NU_HOOK,
    };
    print!("{hook}");

    if starship {
        check_starship_config();
    }

    Ok(())
}

const ZSH_HOOK: &str = r#"_vcs_status_precmd() {
  local dir="${VCS_STATUS_DAEMON_DIR:-/tmp/vcs-status-daemon-$USER}"
  local cwd="${PWD:A}"
  local cache="$dir/cache/${cwd//\//%}"
  if [[ -f "$cache" ]]; then
    export VCS_STATUS="$(<"$cache")"
  else
    export VCS_STATUS="$(vcs-status-daemon)"
  fi
}
precmd_functions+=(_vcs_status_precmd)
"#;

const BASH_HOOK: &str = r#"_vcs_status_precmd() {
  local dir="${VCS_STATUS_DAEMON_DIR:-/tmp/vcs-status-daemon-$USER}"
  local cwd
  cwd=$(pwd -P)
  local cache="$dir/cache/${cwd//\//%}"
  if [[ -f "$cache" ]]; then
    export VCS_STATUS="$(<"$cache")"
  else
    export VCS_STATUS="$(vcs-status-daemon)"
  fi
}
PROMPT_COMMAND="_vcs_status_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
"#;

const NU_HOOK: &str = r#"$env.config = ($env.config | upsert hooks.pre_prompt ($env.config.hooks.pre_prompt | append {||
    let dir = if "VCS_STATUS_DAEMON_DIR" in $env { $env.VCS_STATUS_DAEMON_DIR } else { $"/tmp/vcs-status-daemon-($env.USER)" }
    let cache = $"($dir)/cache/((pwd | path expand | str replace --all '/' '%'))"
    if ($cache | path exists) {
        $env.VCS_STATUS = (open --raw $cache | str trim)
    } else {
        $env.VCS_STATUS = (vcs-status-daemon | str trim)
    }
}))
"#;

const FISH_HOOK: &str = r#"function _vcs_status_precmd --on-event fish_prompt
  set -l dir (set -q VCS_STATUS_DAEMON_DIR; and echo $VCS_STATUS_DAEMON_DIR; or echo /tmp/vcs-status-daemon-$USER)
  set -l cwd (pwd -P)
  set -l cache "$dir/cache/"(string replace -a / % $cwd)
  if test -f "$cache"
    set -gx VCS_STATUS (cat "$cache")
  else
    set -gx VCS_STATUS (vcs-status-daemon)
  end
end
"#;

fn find_starship_config() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STARSHIP_CONFIG") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let xdg = home.join(".config").join("starship.toml");
        if xdg.exists() {
            return Some(xdg);
        }
        let xdg2 = home.join(".config").join("starship").join("starship.toml");
        if xdg2.exists() {
            return Some(xdg2);
        }
    }
    None
}

fn check_starship_config() {
    let Some(path) = find_starship_config() else {
        eprintln!("warning: could not find starship.toml (set STARSHIP_CONFIG to its location)");
        return;
    };

    let Ok(contents) = std::fs::read_to_string(&path) else {
        eprintln!(
            "warning: could not read {} (check permissions)",
            path.display()
        );
        return;
    };

    if !contents.contains("[env_var.VCS_STATUS]") {
        eprintln!(
            "warning: {} does not contain [env_var.VCS_STATUS]",
            path.display()
        );
        eprintln!("  Add the following to your starship.toml:");
        eprintln!();
        eprintln!("  [env_var.VCS_STATUS]");
        eprintln!("  format = \"$env_value \"");
    }
}
