use std::{
    env,
    ffi::{CString, OsString},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "vvdoom",
    version,
    about = "Play Doom in Vivido through Vivid Protocol 1.5",
    trailing_var_arg = true
)]
pub struct Args {
    /// Directory containing doom1.wad and the sound directory.
    #[arg(long, value_name = "DIR")]
    pub asset_dir: Option<PathBuf>,

    /// Vivid control endpoint, normally inherited from Vivido, vvssh, or vvmux.
    #[arg(long, env = "VIVID_ENDPOINT_CONTROL")]
    pub control_endpoint: Option<String>,

    /// Optional realtime endpoint used by the live audio track.
    #[arg(long, env = "VIVID_ENDPOINT_REALTIME")]
    pub realtime_endpoint: Option<String>,

    /// Optional bulk endpoint used by the live raster track.
    #[arg(long, env = "VIVID_ENDPOINT_BULK")]
    pub bulk_endpoint: Option<String>,

    /// Print bounded presentation diagnostics.
    #[arg(short, long)]
    pub verbose: bool,

    /// Arguments passed verbatim to Doom, such as -nosound or -iwad.
    #[arg(
        value_name = "DOOM_ARG",
        num_args = 0..,
        allow_hyphen_values = true,
        trailing_var_arg = true
    )]
    pub doom_args: Vec<OsString>,
}

impl Args {
    pub fn validate(&self) -> Result<()> {
        if self.control_endpoint.is_none() {
            bail!(
                "VIVID_ENDPOINT_CONTROL is not set; run vvdoom inside Vivido, through vvssh, or in a Vivid-enabled vvmux pane"
            );
        }
        Ok(())
    }
}

pub fn resolve_asset_dir(asset_dir: Option<PathBuf>) -> Result<PathBuf> {
    let candidate = match asset_dir {
        Some(path) => path,
        None => env::var_os("VVDOOM_ASSET_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                let local = PathBuf::from("assets");
                local.exists().then_some(local)
            })
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")),
    };
    if !candidate.is_dir() {
        bail!("asset directory does not exist: {}", candidate.display());
    }
    candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve asset directory {}", candidate.display()))
}

pub fn build_doom_argv(asset_dir: &Path, doom_args: Vec<OsString>) -> Vec<OsString> {
    let mut argv = Vec::with_capacity(doom_args.len() + 3);
    argv.push(
        env::args_os()
            .next()
            .unwrap_or_else(|| OsString::from("vvdoom")),
    );
    if !has_doom_flag(&doom_args, "-iwad") {
        argv.push(OsString::from("-iwad"));
        argv.push(asset_dir.join("doom1.wad").into_os_string());
    }
    argv.extend(doom_args);
    argv
}

pub fn has_doom_flag(args: &[OsString], flag: &str) -> bool {
    args.iter()
        .any(|arg| arg.to_string_lossy().eq_ignore_ascii_case(flag))
}

pub fn to_c_args(args: &[OsString]) -> Result<Vec<CString>> {
    args.iter()
        .map(|arg| {
            CString::new(arg.to_string_lossy().into_owned())
                .with_context(|| format!("argument contains an interior NUL byte: {arg:?}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            asset_dir: None,
            control_endpoint: Some("unix:/control".into()),
            realtime_endpoint: None,
            bulk_endpoint: None,
            verbose: false,
            doom_args: Vec::new(),
        }
    }

    #[test]
    fn endpoint_is_required() {
        let mut value = args();
        assert!(value.validate().is_ok());
        value.control_endpoint = None;
        assert!(value.validate().is_err());
    }

    #[test]
    fn injects_default_iwad_when_missing() {
        let argv = build_doom_argv(Path::new("/tmp/assets"), vec![OsString::from("-nosound")]);
        assert!(argv.iter().any(|arg| arg == "-iwad"));
        assert!(argv.iter().any(|arg| arg == "/tmp/assets/doom1.wad"));
        assert!(has_doom_flag(&argv, "-nosound"));
    }

    #[test]
    fn preserves_explicit_iwad() {
        let argv = build_doom_argv(
            Path::new("/tmp/assets"),
            vec![OsString::from("-iwad"), OsString::from("other.wad")],
        );
        assert_eq!(argv.iter().filter(|arg| *arg == "-iwad").count(), 1);
        assert!(argv.iter().any(|arg| arg == "other.wad"));
    }
}
