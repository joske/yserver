use std::{env, process::ExitCode};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if let Some(result) = yserver::internal_probe::run_reexec_helper_if_requested() {
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                log::error!("yserver internal PRIME probe helper: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let opts = match yserver::launch::parse_args(env::args().skip(1)) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("yserver: {err}");
            eprintln!(
                "usage: yserver [:N | N] [vtN] [-seat NAME] [-auth FILE] \
                 [-displayfd N] [-nolisten PROTO] [-novtswitch] [--version]"
            );
            return ExitCode::FAILURE;
        }
    };

    if opts.show_version {
        println!("{}", yserver::version::line());
        return ExitCode::SUCCESS;
    }

    match yserver::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}
