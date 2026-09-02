mod cli;
mod client;
mod geometry;
mod input;
mod media;
mod runtime;
mod terminal;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Args;

fn main() -> ExitCode {
    terminal::install_panic_restore_hook();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vvdoom: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    args.validate()?;
    let asset_dir = cli::resolve_asset_dir(args.asset_dir.clone())?;
    let sound_enabled = !cli::has_doom_flag(&args.doom_args, "-nosound");
    let doom_argv = cli::build_doom_argv(&asset_dir, args.doom_args.clone());
    let mut c_args = cli::to_c_args(&doom_argv)?;

    // The C engine is single-threaded until doomgeneric_Create returns. Set the private sound
    // lookup path before miniaudio can observe it from the mixer thread.
    unsafe {
        std::env::set_var("VVDOOM_SOUND_DIR", asset_dir.join("sound"));
    }

    runtime::reset_exit_request();
    runtime::install_signal_handlers()?;
    let presentation = media::Presentation::connect(&args, sound_enabled)?;
    let _terminal = terminal::TerminalSession::enter()?;
    runtime::run(&mut c_args, presentation)
}
