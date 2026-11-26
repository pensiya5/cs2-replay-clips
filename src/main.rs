use std::process::ExitCode;

#[cfg(windows)]
mod clip_export;
#[cfg(any(windows, test))]
mod episodes;
#[cfg(windows)]
mod export_audio;
#[cfg(windows)]
mod hud;
#[cfg(windows)]
mod mp4_sink;
#[cfg(windows)]
mod video;

fn main() -> ExitCode {
    #[cfg(windows)]
    let result = clip_export::run(std::env::args_os().skip(1));
    #[cfg(not(windows))]
    let result: Result<(), String> = Err("Экспорт MP4 требует Windows Media Foundation".into());
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cs2-replay-clips: {error}");
            ExitCode::FAILURE
        }
    }
}
