use lofi_error::{Error, Result};

use crate::ctx::{ExecCtx, ExecOptions, ExecResult};
use crate::memory;
use crate::run::exec;

/// Long-lived owner of the sandbox thread.
#[derive(Clone)]
pub struct ExecWorker {
    tx: tokio::sync::mpsc::Sender<ExecJob>,
}

struct ExecJob {
    code: String,
    ctx: ExecCtx,
    opts: ExecOptions,
    done: tokio::sync::oneshot::Sender<Result<ExecResult>>,
}

impl ExecWorker {
    /// Spawn a worker that exits when all handles are dropped.
    #[must_use]
    pub fn spawn() -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ExecJob>(1);
        std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    while let Some(job) = rx.recv().await {
                        let _release = memory::ReleaseFreedMemoryOnDrop;
                        if job.done.is_closed() {
                            continue;
                        }
                        let _ = job.done.send(exec(&job.code, &job.ctx, &job.opts).await);
                    }
                });
            }
        });
        Self { tx }
    }

    ///
    /// # Errors
    /// Returns a sandbox error when the worker cannot run the request.
    pub async fn exec(&self, code: &str, ctx: ExecCtx, opts: ExecOptions) -> Result<ExecResult> {
        let (done, done_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(ExecJob {
                code: code.to_string(),
                ctx,
                opts,
                done,
            })
            .await
            .map_err(|_| Error::Sandbox("sandbox worker is gone".to_string()))?;
        done_rx
            .await
            .map_err(|_| Error::Sandbox("sandbox worker dropped the request".to_string()))?
    }
}
