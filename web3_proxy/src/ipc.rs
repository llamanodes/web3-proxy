use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::app::Web3ProxyApp;
use crate::errors::Web3ProxyResult;

/// Start an ipc server that has no rate limits
pub async fn serve(
    socket_path: PathBuf,
    proxy_app: Arc<Web3ProxyApp>,
    mut shutdown_receiver: broadcast::Receiver<()>,
    shutdown_complete_sender: broadcast::Sender<()>,
) -> Web3ProxyResult<()> {
    todo!();
}
