mod cli;
mod tui;
use pxu::share::proxmox::guests;
use pxu::share::host::{Host, ReadOnlyHost, RealHost};
use cli::args::{parse, Cmd, USAGE};
use std::process::ExitCode;

/// El host de producción siempre va envuelto en `ReadOnlyHost`; `refresh` solo habilita el sondeo con refresco.
fn make_host(mock: Option<String>, refresh: bool) -> Result<Box<dyn Host>, String> {
    fn wrap<H: Host + 'static>(h: H, refresh: bool) -> Box<dyn Host> {
        Box::new(if refresh { ReadOnlyHost::allowing_refresh(h) } else { ReadOnlyHost::new(h) })
    }
    match mock {
        None => Ok(wrap(RealHost::new(), refresh)),
        #[cfg(any(debug_assertions, feature = "mock"))]
        Some(name) => {
            use pxu::share::mock::scenario::World;
            let w = World::preset(&name).ok_or_else(|| format!("--mock {name}: unknown case (available: {})", World::preset_names().join(", ")))?;
            Ok(wrap(w.build(), refresh))
        }
        #[cfg(not(any(debug_assertions, feature = "mock")))]
        Some(_) => Err("this build has no mock support (build with --features mock)".into()),
    }
}

// M2: un binario de release con MockHost no puede pisar `target/release/pxu`. Para uno de pruebas: `cargo build --profile mock --features mock`.
#[cfg(all(feature = "mock", not(debug_assertions), not(test)))]
compile_error!("`--release --features mock` is refused: use `cargo build --profile mock --features mock` (target/mock/pxu)");

fn main() -> ExitCode {
    let (cmd, mock) = match parse() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("pxu: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let refresh = !matches!(cmd, Cmd::Check { refresh: Some(false), .. }); // `pxu check --no-refresh` = host estrictamente solo lectura; la TUI decide por tecla (`c` refresca, `C` no)
    let host = match make_host(mock, refresh) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("pxu: {e}");
            return ExitCode::from(2);
        }
    };
    match cmd {
        Cmd::List { json, all_nodes } => {
            let inv = guests::load(host.as_ref());
            if let Some(e) = &inv.config_error {
                eprintln!("pxu: warning: {e}");
            }
            if json {
                println!("{}", cli::list_json(&inv, all_nodes));
            } else {
                print!("{}", cli::list_text(&inv, all_nodes));
            }
            ExitCode::SUCCESS
        }
        Cmd::Check { only, refresh, json, notify } => ExitCode::from(cli::cmd_check(host.as_ref(), &only, refresh, json, notify) as u8),
        Cmd::Update { only, json, yes, notify, host: true } => ExitCode::from(cli::cmd_update_host(host.as_ref(), &only, json, yes, notify) as u8),
        Cmd::Update { only, json, yes, notify, host: false } => ExitCode::from(cli::cmd_update(host.as_ref(), &only, json, yes, notify) as u8),
        Cmd::ConfigCheck => ExitCode::from(cli::cmd_config_check(host.as_ref()) as u8),
        Cmd::NotifyTest => ExitCode::from(cli::cmd_notify_test(host.as_ref()) as u8),
        Cmd::Tui => match tui::run(host.as_ref(), false) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("pxu: {e}");
                ExitCode::from(1)
            }
        },
    }
}
