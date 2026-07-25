//! spot — Pseudo Indestructible Terminal.
//!
//! A PTY session guardian: it keeps your shell (or zellij, or a build, or
//! whatever you point it at) alive across SSH drops, closed laptops and dead
//! WiFi. It is not a multiplexer and it is not a terminal emulator — one tool,
//! one job.

mod client;
mod cmds;
mod daemon;
mod menu;
mod modes;
mod paths;
mod proto;
mod pty;
mod ring;
mod term;
mod update;

use cmds::Options;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const VERSION: &str = concat!("v", env!("CARGO_PKG_VERSION"));

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();

    // argv[0] dispatch: the installer symlinks `stay` -> `spot`, so the bare word
    // works from anywhere PATH reaches — including `:!stay` inside an editor and
    // inside a zellij pane.
    let called_as = argv
        .first()
        .map(|a| {
            PathBuf::from(a)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let args: Vec<String> = if called_as == "stay" {
        let mut v = vec!["stay".to_string()];
        v.extend(argv.into_iter().skip(1));
        v
    } else {
        argv.into_iter().skip(1).collect()
    };

    ExitCode::from(dispatch(args) as u8)
}

fn dispatch(args: Vec<String>) -> i32 {
    // `--help` anywhere before a `--` means help, whatever the subcommand:
    // `stay --help` and `drop --help` should not be read as session names.
    // Scanning stops at `--` so `spot dev -- mycmd --help` still runs mycmd.
    if args
        .iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "--help" || a == "-h")
    {
        print_help();
        return 0;
    }

    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("spot {VERSION}");
            update::flush();
            0
        }
        Some("--help" | "-h" | "help") => {
            print_help();
            0
        }
        Some("--init") => {
            print_shell_init();
            0
        }
        Some("--update") => update::run_foreground(VERSION),
        Some("--self-update-bg") => {
            update::run_background(VERSION);
            0
        }
        Some("--daemon") => run_daemon(&args[1..]),
        Some("ls" | "ps") => with_dir(cmds::ls),
        Some("stay") => {
            let name = args.get(1).cloned();
            with_dir(|dir| {
                if let Some(n) = &name {
                    if !paths::valid_name(n) {
                        eprintln!("spot: invalid session name '{n}'");
                        return 1;
                    }
                }
                cmds::stay(dir, name.as_deref())
            })
        }
        Some("drop") => {
            let Some(name) = args.get(1).cloned() else {
                eprintln!("spot: drop needs a session name");
                return 1;
            };
            let force = args.iter().any(|a| a == "--force" || a == "-f");
            with_dir(|dir| {
                if !paths::valid_name(&name) {
                    eprintln!("spot: invalid session name '{name}'");
                    return 1;
                }
                cmds::drop_session(dir, &name, force, Duration::from_secs(5))
            })
        }
        Some("fetch" | "attach") => {
            let Some(name) = args.get(1).cloned() else {
                eprintln!("spot: fetch needs a session name");
                return 1;
            };
            let opts = Options {
                steal: args.iter().any(|a| a == "--steal"),
                ..Default::default()
            };
            if args[0] == "attach" {
                // `attach` is the create-if-missing spelling; `fetch` is strict.
                return with_dir(|dir| start(dir, &name, &opts, true));
            }
            with_dir(|dir| start(dir, &name, &opts, false))
        }
        Some(other) if other.starts_with('-') => {
            eprintln!("spot: unknown option '{other}'\n");
            print_help();
            1
        }
        Some(name) => {
            let name = name.to_string();
            let rest: Vec<String> = args.iter().skip(1).cloned().collect();
            let opts = parse_session_opts(&rest);
            with_dir(|dir| start(dir, &name, &opts, true))
        }
        None => with_dir(picker),
    }
}

fn start(dir: &std::path::Path, name: &str, opts: &Options, create: bool) -> i32 {
    if !paths::valid_name(name) {
        eprintln!(
            "spot: invalid session name '{name}' \
             (letters, digits, '.', '_', '-'; must start alphanumeric)"
        );
        return 1;
    }
    if create {
        cmds::attach_or_create(dir, name, opts)
    } else {
        cmds::fetch(dir, name, opts)
    }
}

fn parse_session_opts(rest: &[String]) -> Options {
    let mut opts = Options::default();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--" => {
                opts.command = rest[i + 1..].to_vec();
                break;
            }
            "--steal" => opts.steal = true,
            "--keep-on-exit" => opts.keep_on_exit = true,
            "--ring-bytes" => {
                if let Some(v) = rest.get(i + 1).and_then(|v| v.parse().ok()) {
                    opts.ring_bytes = v;
                    i += 1;
                }
            }
            other => eprintln!("spot: ignoring unknown option '{other}'"),
        }
        i += 1;
    }
    opts
}

fn with_dir(f: impl FnOnce(&std::path::Path) -> i32) -> i32 {
    match paths::runtime_dir() {
        Ok(d) => f(&d),
        Err(e) => {
            eprintln!("spot: {e}");
            1
        }
    }
}

/// `spot` with no arguments: the login picker.
fn picker(dir: &std::path::Path) -> i32 {
    // Never hijack a non-interactive shell (scp, rsync, scripts).
    if !menu::interactive() {
        return 0;
    }
    // Already inside a session: hand straight back to the shell. This is what
    // prevents spot-in-spot.
    if std::env::var_os("SPOT_SOCKET").is_some() {
        return 0;
    }
    update::maybe_check_in_background();

    // Only detached sessions are attachable, so only they are offered.
    let all = cmds::sessions(dir);
    let attachable: Vec<cmds::Status> = all.into_iter().filter(|s| !s.attached).collect();

    match menu::run(&attachable, VERSION) {
        Ok(menu::Choice::Shell) => 0,
        Ok(menu::Choice::Attach(name)) => {
            let opts = Options::default();
            cmds::attach_or_create(dir, &name, &opts)
        }
        Ok(menu::Choice::NewSession) => {
            let name = menu::prompt_name().unwrap_or_else(paths::codename);
            if !paths::valid_name(&name) {
                eprintln!("spot: invalid session name '{name}'");
                return 1;
            }
            let opts = Options::default();
            cmds::attach_or_create(dir, &name, &opts)
        }
        Err(e) => {
            eprintln!("spot: {e} — continuing to shell.");
            0
        }
    }
}

/// `spot --daemon <name> --ring N --size CxR [--keep-on-exit] [-- argv...]`
fn run_daemon(args: &[String]) -> i32 {
    let Some(name) = args.first().cloned() else {
        return 1;
    };
    let mut ring = 64 * 1024usize;
    let mut keep = false;
    let mut size = (80u16, 24u16);
    let mut command: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ring" => {
                if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                    ring = v;
                }
                i += 1;
            }
            "--size" => {
                if let Some(v) = args.get(i + 1) {
                    if let Some((c, r)) = v.split_once('x') {
                        size = (c.parse().unwrap_or(80), r.parse().unwrap_or(24));
                    }
                }
                i += 1;
            }
            "--keep-on-exit" => keep = true,
            "--" => {
                command = args[i + 1..].to_vec();
                break;
            }
            _ => {}
        }
        i += 1;
    }

    let argv = if command.is_empty() {
        default_shell()
    } else {
        command
    };
    let sock = paths::runtime_dir()
        .map(|d| paths::socket_path(&d, &name))
        .unwrap_or_default();
    let env = vec![
        (
            "SPOT_SOCKET".to_string(),
            sock.to_string_lossy().into_owned(),
        ),
        ("SPOT_SESSION".to_string(), name.clone()),
    ];
    daemon::run(name, argv, env, keep, ring, (size.0, size.1, 0, 0));
}

/// `$SHELL` as a login shell, per screen/tmux convention. Recursion is not a
/// concern: the `--init` snippet skips when `SPOT_SOCKET` is set.
fn default_shell() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let base = PathBuf::from(&shell)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sh".to_string());
    // argv[0] with a leading '-' is what makes it a login shell.
    vec![shell, format!("-{base}")]
}

fn print_help() {
    println!(
        "spot {VERSION} — Pseudo Indestructible Terminal

USAGE:
    spot                     Interactive session picker
    spot <name>              Attach to <name>, creating it if needed
    spot fetch <name>        Attach only; error if it does not exist
    spot stay [name]         Detach: unhook the client, leave the session running
    spot ls                  List sessions (alias: ps)
    spot drop <name>         Terminate a session's child process

OPTIONS (session creation):
    --                       Everything after this is the command to run
    --steal                  Take over a session that is already attached
    --keep-on-exit           Hold the session after the child exits, to report status
    --ring-bytes N           Replay buffer size (default 65536, 0 disables)
    --force, -f              drop: SIGKILL immediately, no grace period

OTHER:
    spot --init              Print the shell snippet for your rc file
    spot --update            Check for and install the latest release
    spot --version           Print version

Inside a session, `stay` detaches it. In the picker: ↑/↓ move, enter selects,
esc drops to the shell."
    );
}

/// The rc snippet. Guards against non-interactive shells and, crucially, against
/// spot-in-spot via `$SPOT_SOCKET`.
fn print_shell_init() {
    print!(
        r#"# >>> spot >>>
# Launch spot on interactive login shells (skips inside spot & non-tty).
if command -v spot >/dev/null 2>&1; then
  case $- in
    *i*)
      if [ -z "$SPOT_SOCKET" ] && [ -t 1 ]; then
        spot
      fi
      ;;
  esac
  # Fallback for hosts where the `stay` symlink was not installed.
  command -v stay >/dev/null 2>&1 || stay() {{ spot stay "$@"; }}
fi
# <<< spot <<<
"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shell_is_a_login_shell() {
        std::env::set_var("SHELL", "/bin/zsh");
        let v = default_shell();
        assert_eq!(v[0], "/bin/zsh");
        assert_eq!(
            v[1], "-zsh",
            "argv[0] must be dash-prefixed for a login shell"
        );
    }

    #[test]
    fn session_opts_take_a_command_after_double_dash() {
        let rest: Vec<String> = ["--keep-on-exit", "--", "cargo", "watch", "-x", "test"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_session_opts(&rest);
        assert!(o.keep_on_exit);
        assert_eq!(o.command, vec!["cargo", "watch", "-x", "test"]);
    }

    #[test]
    fn ring_bytes_parses() {
        let rest: Vec<String> = ["--ring-bytes", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(parse_session_opts(&rest).ring_bytes, 0);
    }
}
