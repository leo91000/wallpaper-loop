use std::{
    env, fs,
    io::{self, ErrorKind},
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            fs::MetadataExt,
            net::{SocketAddr, UnixDatagram},
        },
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};

use crate::desktop::Desktop;

pub struct PauseSocket {
    socket: UnixDatagram,
    path: PathBuf,
    identity: (u64, u64),
}

impl PauseSocket {
    pub fn install(path: &Path, handle: LoopHandle<'_, Desktop>) -> Result<()> {
        let socket = UnixDatagram::bind(path)
            .with_context(|| format!("bind pause socket {}", path.display()))?;
        let metadata = fs::symlink_metadata(path)?;
        let source = Self {
            socket,
            path: path.to_owned(),
            identity: (metadata.dev(), metadata.ino()),
        };
        source.socket.set_nonblocking(true)?;
        handle
            .insert_source(
                Generic::new(source, Interest::READ, Mode::Level),
                |_, source, desktop| source.receive(desktop),
            )
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn receive(&self, desktop: &mut Desktop) -> io::Result<PostAction> {
        let mut bytes = [0; 256];
        for _ in 0..64 {
            match self.socket.recv(&mut bytes) {
                Ok(size) if size < bytes.len() => {
                    if let Ok(payload) = std::str::from_utf8(&bytes[..size])
                        && let Some((output, paused)) = parse_pause(payload)
                    {
                        desktop.set_paused(output, paused);
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(PostAction::Continue)
    }
}

impl std::os::fd::AsFd for PauseSocket {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

impl Drop for PauseSocket {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn parse_pause(payload: &str) -> Option<(&str, bool)> {
    let (command, output) = payload.split_once('\t')?;
    if output.is_empty() || output.contains(['\t', '\n', '\r', '\0']) {
        return None;
    }
    match command {
        "pause" => Some((output, true)),
        "resume" => Some((output, false)),
        _ => None,
    }
}

pub fn notify_address() -> Result<Option<SocketAddr>> {
    env::var_os("NOTIFY_SOCKET")
        .map(|socket| {
            let bytes = socket.as_bytes();
            if let Some(name) = bytes.strip_prefix(b"@") {
                SocketAddr::from_abstract_name(name)
            } else {
                SocketAddr::from_pathname(Path::new(&socket))
            }
        })
        .transpose()
        .context("read systemd notify socket")
}

pub fn notify_ready(address: &SocketAddr) -> Result<()> {
    UnixDatagram::unbound()?
        .send_to_addr(b"READY=1", address)
        .context("report wallpaper startup to systemd")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_one_output_per_message() {
        assert_eq!(super::parse_pause("pause\tDP-1"), Some(("DP-1", true)));
        assert_eq!(super::parse_pause("resume\tDP-2"), Some(("DP-2", false)));
        assert_eq!(super::parse_pause("pause\tDP-1\nDP-2"), None);
        assert_eq!(super::parse_pause("pause\t"), None);
        assert_eq!(super::parse_pause("status\tDP-1"), None);
    }
}
