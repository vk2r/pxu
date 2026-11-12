//! Argumentos de la línea de comandos.
pub const USAGE: &str = "usage: pxu [--mock CASE] [list|check|update|config check|config encrypt-secrets|notify-test|notify-flush] [options]\n  pxu                      open the TUI\n  pxu list [--json] [--all-nodes]\n                           one line per LXC/VM (config read from /etc/pve, no processes)\n  pxu check [--json] [--no-refresh] [--notify] [--ct 101,103] [vmid...]\n                           updates per LXC, one exec per CT; refreshes the package indexes by default\n                           ([check] refresh = false turns it off; --no-refresh = read-only check; --refresh kept)\n                           exit: 0 none · 10 updates · 12 a CT failed · 2 usage/config\n  pxu update [--json] [--yes] [--no-notify] [--ct 101,103] [vmid...]
                           upgrade CTs one at a time (temporary backup, simulate, upgrade, verify; automatic rollback\n                           to the backup if it fails, then the backup is deleted). Only CTs with a
                           positive recent `pxu check`; default: CTs with auto_update. The only command that writes.
                           exit: 0 ok · 20 a CT failed · 30 stopped (2 failures in a row / Ctrl-C) · 3 busy · 2 usage
  pxu update --host [--yes] install the Proxmox HOST updates (apt full-upgrade) after a confirmation (the manual way).
                           Needs a terminal (never unattended, --yes included: exit 2 without one); --yes skips the question.
                           Backs up /etc (and snapshots a ZFS root) first. Same exit codes.\n                           With `[update] host_auto = true`, a plain `pxu update` (and the timer) updates the host FIRST, and with\n                           `host_nvidia_auto` also the NVIDIA .run driver (same branch); CTs only if the host ended well. Never reboots.
  pxu config check         validate /etc/pxu/config.toml (TOML, values, mode 0600, owner root)
  pxu config encrypt-secrets
                           encrypt a plain-text [apprise] api_key / urls with systemd-creds (same as saving them in the TUI: Config → N)
  pxu notify-test          send a test notice via [apprise] (exit 5 if the send fails)
  pxu notify-flush         resend the queued notices (/var/lib/pxu/outbox) and exit; no network if the queue is empty
                           (the pxu-notify-flush timer runs it every 15 min). exit: 0 queue empty · 5 notices still queued\n";

pub enum Cmd {
    Tui,
    List { json: bool, all_nodes: bool },
    Check { only: Vec<u32>, refresh: Option<bool>, json: bool, notify: bool },
    Update { only: Vec<u32>, json: bool, yes: bool, notify: bool, host: bool },
    ConfigCheck,
    ConfigEncryptSecrets,
    NotifyTest,
    NotifyFlush,
}

pub fn parse() -> Result<(Cmd, Option<String>), lexopt::Error> {
    parse_from(lexopt::Parser::from_env())
}

fn parse_from(mut p: lexopt::Parser) -> Result<(Cmd, Option<String>), lexopt::Error> {
    use lexopt::prelude::*;
    let mut mock = std::env::var("PXU_MOCK").ok();
    let (mut json, mut all_nodes, mut cmd) = (false, false, None::<&'static str>);
    let (mut refresh, mut notify, mut only) = (None::<bool>, false, Vec::<u32>::new());
    let (mut yes, mut no_notify, mut want_check, mut want_encrypt, mut host_flag) = (false, false, false, false, false);
    while let Some(a) = p.next()? {
        match a {
            Long("mock") => mock = Some(p.value()?.string()?),
            Long("json") => json = true,
            Long("all-nodes") => all_nodes = true,
            Long("refresh") => refresh = Some(true),
            Long("no-refresh") => refresh = Some(false),
            Long("notify") => notify = true,
            Long("yes") | Short('y') => yes = true,
            Long("no-notify") => no_notify = true,
            Long("host") => host_flag = true,
            Long("ct") => {
                for v in p.value()?.string()?.split(',') {
                    only.push(v.trim().parse().map_err(|_| lexopt::Error::from(format!("--ct: `{v}` is not a vmid")))?);
                }
            }
            Short('h') | Long("help") => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            Value(v) if cmd.is_none() => match v.to_str() {
                Some("list") => cmd = Some("list"),
                Some("check") => cmd = Some("check"),
                Some("update") => cmd = Some("update"),
                Some("config") => cmd = Some("config"),
                Some("notify-test") => cmd = Some("notify-test"),
                Some("notify-flush") => cmd = Some("notify-flush"),
                _ => return Err(lexopt::Error::UnexpectedArgument(v)),
            },
            Value(v) if cmd == Some("config") && v.to_str() == Some("check") => want_check = true,
            Value(v) if cmd == Some("config") && v.to_str() == Some("encrypt-secrets") => want_encrypt = true,
            Value(v) if cmd == Some("check") || cmd == Some("update") => only.push(v.string()?.parse().map_err(|_| lexopt::Error::from("vmid must be a number".to_string()))?),
            _ => return Err(a.unexpected()),
        }
    }
    let c = match cmd {
        Some("list") => Cmd::List { json, all_nodes },
        Some("check") => Cmd::Check { only, refresh, json, notify },
        Some("update") => Cmd::Update { only, json, yes, notify: !no_notify, host: host_flag },
        Some("config") if want_check => Cmd::ConfigCheck,
        Some("config") if want_encrypt => Cmd::ConfigEncryptSecrets,
        Some("config") => return Err(lexopt::Error::from("config: expected `check` or `encrypt-secrets`".to_string())),
        Some("notify-test") => Cmd::NotifyTest,
        Some("notify-flush") => Cmd::NotifyFlush,
        _ => Cmd::Tui,
    };
    Ok((c, mock))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Result<(Cmd, Option<String>), lexopt::Error> {
        parse_from(lexopt::Parser::from_args(a.iter().map(|s| s.to_string())))
    }

    #[test]
    fn notify_flush_parses_and_rejects_extra_arguments() {
        assert!(matches!(args(&["notify-flush"]), Ok((Cmd::NotifyFlush, _))));
        assert!(args(&["notify-flush", "extra"]).is_err());
        assert!(USAGE.contains("notify-flush"));
    }
}
