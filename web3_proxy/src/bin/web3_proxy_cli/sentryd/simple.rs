use std::time::Duration;

use super::{SentrydErrorBuilder, SentrydResult};
use anyhow::Context;
use tokio::time::Instant;
use tracing::{debug, trace};

/// GET the url and return an error if it wasn't a success
pub async fn main(
    error_builder: SentrydErrorBuilder,
    url: String,
    timeout: Duration,
) -> SentrydResult {
    let start = Instant::now();

    let r = reqwest::get(&url)
        .await
        .context(format!("Failed GET {}", &url))
        .map_err(|x| error_builder.build(x))?;

    let elapsed = start.elapsed();

    if elapsed > timeout {
        return error_builder.result(
            anyhow::anyhow!(
                "query took longer than {}ms ({}ms): {:#?}",
                timeout.as_millis(),
                elapsed.as_millis(),
                r
            )
            .context(format!("fetching {} took too long", &url)),
        );
    }

    // TODO: what should we do if we get rate limited here?

    if r.status().is_success() {
        debug!("{} is healthy", &url);
        trace!("Successful {:#?}", r);
        return Ok(());
    }

    // TODO: capture headers? or is that already part of r?
    let detail = format!("{:#?}", r);

    let summary = format!("{} is unhealthy: {}", &url, r.status());

    let body = r
        .text()
        .await
        .context(detail.clone())
        .context(summary.clone())
        .map_err(|x| error_builder.build(x))?;

    error_builder.result(
        anyhow::anyhow!("body: {}", body)
            .context(detail)
            .context(summary),
    )
}
