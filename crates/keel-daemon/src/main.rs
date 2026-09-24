use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: keel-daemon <dataflow.yml>");
        return ExitCode::FAILURE;
    };
    match keel_daemon::run(Path::new(&path)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("[daemon] error: {e}");
            ExitCode::FAILURE
        }
    }
}
