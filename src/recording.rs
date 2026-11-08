//! Compatibility with the pinned Puppeteer recorder's odd-dimension padding.
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
};

pub fn launcher(profile: &Path, encoder: &Path) -> Result<PathBuf> {
    fn quote(path: &Path) -> Result<String> {
        Ok(format!(
            "'{}'",
            path.to_str()
                .context("recording executable path must be UTF-8")?
                .replace('\'', "'\\''")
        ))
    }
    let script = format!(
        "#!/bin/sh\nexec {} __recording-ffmpeg {} \"$@\"\n",
        quote(&std::env::current_exe()?)?,
        quote(encoder)?
    );
    let directory = super::logging::location(profile, "broker")?
        .parent()
        .context("log directory missing")?
        .to_owned();
    let path = directory.join(format!(
        "recording-{:016x}-{:016x}.sh",
        xxhash_rust::xxh3::xxh3_64(profile.as_os_str().as_encoded_bytes()),
        xxhash_rust::xxh3::xxh3_64(script.as_bytes())
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(script.as_bytes())?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o7777 != 0o700
                || metadata.len() != script.len() as u64
            {
                bail!("invalid existing recording launcher");
            }
            let mut actual = String::new();
            file.read_to_string(&mut actual)?;
            if actual != script {
                bail!("recording launcher content mismatch");
            }
        }
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn even_padding(filter: &str) -> Result<String> {
    let (before, after) = filter
        .split_once(",pad=")
        .context("pinned recorder filter has no padding")?;
    let (padding, suffix) = after.split_once(',').unwrap_or((after, ""));
    let parts: Vec<_> = padding.split(':').collect();
    if parts.len() != 4 || parts[2..] != ["0", "0"] {
        bail!("unexpected pinned recorder padding");
    }
    for dimension in &parts[..2] {
        let number: f64 = dimension
            .parse()
            .context("invalid recorder padding dimension")?;
        if !number.is_finite() || number <= 0.0 {
            bail!("invalid recorder padding dimension");
        }
    }
    Ok(format!(
        "{before},pad=ceil({}/2)*2:ceil({}/2)*2:0:0{}{}",
        parts[0],
        parts[1],
        if suffix.is_empty() { "" } else { "," },
        suffix
    ))
}

pub fn exec(mut args: Vec<OsString>) -> Result<()> {
    if args.is_empty() {
        bail!("recording encoder executable missing");
    }
    let executable = args.remove(0);
    // Puppeteer also invokes the encoder without arguments to check availability.
    if !args.is_empty() {
        let index = args
            .iter()
            .position(|arg| arg == "-vf")
            .context("pinned recording filter missing")?
            + 1;
        let filter = args
            .get(index)
            .and_then(|arg| arg.to_str())
            .context("recording filter missing")?;
        args[index] = even_padding(filter)?.into();
        eprintln!(
            "browser-mcp: rounding recording padding to even dimensions for the pinned recorder"
        );
    }
    Err(std::process::Command::new(executable)
        .args(args)
        .exec()
        .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rounds_only_the_expected_padding_and_preserves_other_filters() {
        assert_eq!(
            even_padding("crop='min(1265,iw):min(633,ih):0:0',pad=1265:633:0:0,scale=iw/2:-1")
                .unwrap(),
            "crop='min(1265,iw):min(633,ih):0:0',pad=ceil(1265/2)*2:ceil(633/2)*2:0:0,scale=iw/2:-1"
        );
        assert!(even_padding("pad=100:100:0:0").is_err());
        assert!(even_padding("crop=x,pad=NaN:100:0:0").is_err());
    }
}
