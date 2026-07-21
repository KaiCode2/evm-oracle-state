//! Flashbots MEV-Share SSE source for transaction and bundle hints.

use std::{fmt, str::FromStr, time::Duration};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::watch;

use super::{
    ETHEREUM_MAINNET_CHAIN_ID, PendingOracleCandidateSource, PendingOracleOrderingHandle,
    PendingOracleSource, PendingOracleSourceDescriptor, PendingOracleSourceError,
    PendingOracleSourceFuture, PendingOracleSourceId, PendingOracleSourceSink,
    PendingOracleTransmissionId, PendingTransportCandidate,
};

/// Flashbots MEV-Share event-stream source.
#[derive(Clone)]
pub struct MevSharePendingTransactionSource {
    stream_url: String,
    source_id: PendingOracleSourceId,
    chain_id: u64,
}

impl MevSharePendingTransactionSource {
    /// Construct a source for an MEV-Share SSE endpoint.
    pub fn new(stream_url: impl Into<String>) -> Self {
        Self {
            stream_url: stream_url.into(),
            source_id: PendingOracleSourceId::new("flashbots-mev-share"),
            chain_id: ETHEREUM_MAINNET_CHAIN_ID,
        }
    }

    /// Construct a source for the public Ethereum MEV-Share event stream.
    pub fn ethereum_mainnet() -> Self {
        Self::new("https://mev-share.flashbots.net")
    }

    /// Override the stable source identifier used in health and provenance.
    pub fn source_id(mut self, source_id: PendingOracleSourceId) -> Self {
        self.source_id = source_id;
        self
    }

    /// Set the declared chain id. It must match the pending runtime configuration.
    pub fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = chain_id;
        self
    }

    async fn run_forever(
        self,
        sink: PendingOracleSourceSink,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), PendingOracleSourceError> {
        let client = reqwest::Client::builder()
            .user_agent("evm-oracle-state/pending-oracle-mev-share")
            .build()
            .map_err(transport_error)?;
        let mut retry = Duration::from_secs(1);
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            match self.run_connection(&client, &sink, &mut shutdown).await {
                Ok(()) if *shutdown.borrow() => return Ok(()),
                Ok(()) => sink.coverage_gap("MEV-Share event stream ended"),
                Err(error) => sink.coverage_gap(error.to_string()),
            }
            sink.reconnecting();
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                () = tokio::time::sleep(retry) => {}
            }
            retry = (retry * 2).min(Duration::from_secs(30));
        }
    }

    async fn run_connection(
        &self,
        client: &reqwest::Client,
        sink: &PendingOracleSourceSink,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), PendingOracleSourceError> {
        let response = client
            .get(&self.stream_url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(transport_error)?
            .error_for_status()
            .map_err(transport_error)?;
        sink.ready();
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        return Ok(());
                    }
                }
                chunk = stream.next() => {
                    let Some(chunk) = chunk else {
                        return Err(PendingOracleSourceError::Transport(
                            "MEV-Share response body closed".to_string(),
                        ));
                    };
                    let chunk = chunk.map_err(transport_error)?;
                    sink.transport_message();
                    buffer.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
                    while let Some(boundary) = buffer.find("\n\n") {
                        let remainder = buffer.split_off(boundary + 2);
                        let frame = std::mem::replace(&mut buffer, remainder);
                        let frame = &frame[..boundary];
                        let Some(event) = parse_sse_frame(frame) else {
                            continue;
                        };
                        for candidate in self.event_candidates(&event) {
                            if sink
                                .runtime()
                                .interests()
                                .iter()
                                .any(|interest| interest.matches(&candidate))
                            {
                                sink.candidate();
                                match sink.runtime().observe_candidate(candidate) {
                                    Ok(report) => {
                                        for failure in report.failures {
                                            tracing::debug!(
                                                adapter_id = %failure.adapter_id,
                                                error = %failure.error,
                                                "pending oracle adapter rejected MEV-Share candidate"
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        tracing::debug!(%error, "pending oracle runtime rejected MEV-Share candidate");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn event_candidates(&self, event: &Value) -> Vec<PendingTransportCandidate> {
        let Some(hash) = event
            .get("hash")
            .and_then(Value::as_str)
            .and_then(|value| B256::from_str(value).ok())
        else {
            return Vec::new();
        };
        event
            .get("txs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, transaction)| {
                let to = transaction
                    .get("to")
                    .and_then(Value::as_str)
                    .and_then(|value| Address::from_str(value).ok())?;
                let calldata = transaction
                    .get("callData")
                    .and_then(Value::as_str)
                    .and_then(|value| Bytes::from_str(value).ok())?;
                if calldata.len() < 4 {
                    return None;
                }
                let mut identity = Vec::with_capacity(32 + 8 + 20 + calldata.len());
                identity.extend_from_slice(hash.as_slice());
                identity.extend_from_slice(&(index as u64).to_be_bytes());
                identity.extend_from_slice(to.as_slice());
                identity.extend_from_slice(&calldata);
                Some(PendingTransportCandidate::new(
                    self.chain_id,
                    PendingOracleTransmissionId::from_hash(keccak256(identity)),
                    PendingOracleSource::MevShare,
                    self.source_id.clone(),
                    to,
                    calldata,
                    PendingOracleOrderingHandle::MevShare { hash },
                    None,
                ))
            })
            .collect()
    }
}

impl fmt::Debug for MevSharePendingTransactionSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MevSharePendingTransactionSource")
            .field("stream_url", &"<redacted>")
            .field("source_id", &self.source_id)
            .field("chain_id", &self.chain_id)
            .finish()
    }
}

impl PendingOracleCandidateSource for MevSharePendingTransactionSource {
    fn descriptor(&self) -> PendingOracleSourceDescriptor {
        PendingOracleSourceDescriptor::new(PendingOracleSource::MevShare, self.source_id.clone())
    }

    fn run(
        self: Box<Self>,
        sink: PendingOracleSourceSink,
        shutdown: watch::Receiver<bool>,
    ) -> PendingOracleSourceFuture {
        Box::pin(async move { self.run_forever(sink, shutdown).await })
    }
}

fn parse_sse_frame(frame: &str) -> Option<Value> {
    let payload = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    (!payload.is_empty())
        .then(|| serde_json::from_str(&payload).ok())
        .flatten()
}

fn transport_error(error: impl fmt::Display) -> PendingOracleSourceError {
    PendingOracleSourceError::Transport(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_sse_frame;

    #[test]
    fn parses_multiline_sse_data() {
        let parsed = parse_sse_frame("event: transaction\ndata: {\"hash\":\ndata: \"0x01\"}")
            .expect("valid SSE payload");
        assert_eq!(parsed["hash"], "0x01");
    }
}
