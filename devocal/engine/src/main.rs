//! `devocal-engine.exe --app-pid <u32> --restore-file <path>`: the audio engine process the
//! app controls over `\\.\pipe\tune-love-devocal-<app pid>` (see `engine::run`).

// Some building blocks keep test-only or not-yet-used helpers.
#[allow(dead_code)]
mod audio;
#[allow(dead_code)]
mod dsp;
mod engine;
#[allow(dead_code)]
mod holder;
#[allow(dead_code)]
mod load;
mod notify;
mod pipe;
#[allow(dead_code)]
mod processor;
#[allow(dead_code)]
mod separator;
mod state;
#[allow(dead_code)]
mod stemgen;

fn main() {
    let args = match engine::parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("devocal-engine: {e}\n{}", engine::USAGE);
            std::process::exit(2);
        }
    };
    // The main loop uses WinSessions (COM) on this thread.
    let code = match audio::ComGuard::init_mta() {
        Ok(com) => {
            let code = engine::run(args);
            drop(com);
            code
        }
        Err(e) => {
            eprintln!("devocal-engine: {e}");
            1
        }
    };
    std::process::exit(code);
}
