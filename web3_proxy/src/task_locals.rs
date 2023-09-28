use crate::app::Web3ProxyApp;
use std::sync::Arc;
use tokio::task_local;

task_local! {
    pub static APP: Arc<Web3ProxyApp>;
}
