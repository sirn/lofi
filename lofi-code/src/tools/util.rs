//! Shared runtime helpers for the sandbox tools: a process-group kill
//! guard ([`PgrpKillGuard`]) and a capped async reader ([`read_capped`]).

use tokio::io::AsyncReadExt;

pub struct PgrpKillGuard {
    pid: Option<u32>,
}

impl PgrpKillGuard {
    #[allow(clippy::must_use_candidate)]
    pub fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }
    pub fn disarm(&mut self) {
        self.pid = None;
    }
}

#[allow(clippy::cast_possible_wrap)]
impl Drop for PgrpKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            // `kill(-pgid, SIGKILL)` signals the whole process group. The
            // child was made group leader by `process_group(0)`, so its pid
            // is the group id. `nix` wraps the FFI behind a safe API.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-(pid as i32)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

#[allow(clippy::missing_errors_doc)]
pub async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        let room = cap.saturating_sub(buf.len());
        if room == 0 {
            drain(r).await?;
            return Ok((buf, true));
        }
        let take = n.min(room);
        buf.extend_from_slice(&tmp[..take]);
        if take < n {
            drain(r).await?;
            return Ok((buf, true));
        }
    }
    Ok((buf, false))
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<()> {
    let mut tmp = [0u8; 8192];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
    }
    Ok(())
}
