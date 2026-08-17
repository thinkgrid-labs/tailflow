pub mod docker;
pub mod file;
pub mod process;
pub mod stdin;

use crate::LogSender;
use anyhow::Result;
use tokio_util::sync::CancellationToken;

/// Every ingestion source implements this trait.
#[async_trait::async_trait]
pub trait Source: Send + 'static {
    /// Name shown in the UI (e.g. container name, file path, "stdin").
    fn name(&self) -> &str;

    /// Spawn the ingestion loop and push records onto `tx`.
    /// The implementation must stop when `shutdown` is cancelled or when the
    /// output bus is dropped. A daemon owns the lifecycle of every source it
    /// starts; no child process or blocking watcher should survive it.
    async fn run(self: Box<Self>, tx: LogSender, shutdown: CancellationToken) -> Result<()>;
}
