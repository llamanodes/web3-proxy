use anyhow::Context;
use log::{debug, trace};

/// GET the url and return an error if it wasn't a success
pub async fn main(url: String) -> anyhow::Result<()> {
    let r = reqwest::get(&url)
        .await
        .context(format!("Failed GET {}", url))?;

    if r.status().is_success() {
        // warn if latency is high?
        debug!("{} is healthy", url);
        trace!("Successful {:#?}", r);
        return Ok(());
    }

    let debug_str = format!("{:#?}", r);

    let body = r.text().await?;

    Err(anyhow::anyhow!("{}: {}", debug_str, body))
}
