use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};

use crate::ApiError;

/// POST JSON and return a stream of (event, data) SSE pairs.
pub async fn post_sse(
    req: reqwest::RequestBuilder,
) -> anyhow::Result<impl Stream<Item = anyhow::Result<(String, String)>>> {
    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError { status: status.as_u16(), body }.into());
    }
    Ok(resp.bytes_stream().eventsource().map(|r| match r {
        Ok(ev) => Ok((ev.event, ev.data)),
        Err(e) => Err(anyhow::anyhow!("stream error: {e}")),
    }))
}
