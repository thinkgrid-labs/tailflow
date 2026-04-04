pub mod docker;
pub mod file;
pub mod stdin;

use crate::LogSender;
use anyhow::Result;

/// Every ingestion source implements this trait.
#[async_trait::async_trait]
pub trait Source: Send + 'static {
    /// Name shown in the UI (e.g. container name, file path, "stdin").
    fn name(&self) -> &str;

    /// Spawn the ingestion loop and push records onto `tx`.
    /// The implementation is responsible for exiting when `tx` is dropped
    /// (i.e. when `tx.send()` returns `SendError`).
    async fn run(self: Box<Self>, tx: LogSender) -> Result<()>;
}
