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
