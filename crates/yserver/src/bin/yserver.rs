use std::{env, process::ExitCode};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opts = match yserver::launch::parse_args(env::args().skip(1)) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("yserver: {err}");
            eprintln!(
                "usage: yserver [:N | N] [vtN] [-seat NAME] [-auth FILE] \
                 [-displayfd N] [-nolisten PROTO] [-novtswitch]"
            );
            return ExitCode::FAILURE;
        }
    };

    match yserver::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}
