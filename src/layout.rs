use alloy_primitives::{Address, B256};

/// Known aggregator storage layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AggregatorLayout {
    /// Chainlink OCR2 `AccessControlledOCR2Aggregator 1.0.0` compatible layout.
    ChainlinkOcr2V1,
    /// Chainlink OCR1 `AccessControlledOffchainAggregator 2.0.0` compatible layout.
    ChainlinkOcr1V2,
    /// Chainlink OCR1 `AccessControlledOffchainAggregator 3.0.0` compatible layout.
    ChainlinkOcr1V3,
    /// Chainlink OCR1 `AccessControlledOffchainAggregator 4.0.0` compatible layout.
    ChainlinkOcr1V4,
    /// The aggregator exposed a version string, but this crate has no direct
    /// storage adapter for it.
    Unsupported,
    /// No usable layout evidence was available.
    Unknown,
}

/// How an aggregator layout was detected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregatorLayoutConfidence {
    /// The layout came directly from `typeAndVersion()`.
    TypeAndVersion,
    /// The layout came from a previously observed matching code hash.
    CodeHashCache,
    /// No usable evidence was available.
    Unknown,
}

/// Evidence collected while identifying an aggregator implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregatorLayoutEvidence {
    /// Aggregator address that was inspected.
    pub aggregator: Address,
    /// Raw `typeAndVersion()` result, when available.
    pub type_and_version: Option<String>,
    /// Runtime code hash, when available from the provider.
    pub code_hash: Option<B256>,
    /// Classified layout.
    pub layout: AggregatorLayout,
    /// Detection confidence.
    pub confidence: AggregatorLayoutConfidence,
}

impl AggregatorLayoutEvidence {
    /// Build evidence from a `typeAndVersion()` read and optional code hash.
    pub fn from_type_and_version(
        aggregator: Address,
        type_and_version: impl Into<String>,
        code_hash: Option<B256>,
    ) -> Self {
        let type_and_version = type_and_version.into();
        Self {
            aggregator,
            layout: classify_type_and_version(&type_and_version),
            type_and_version: Some(type_and_version),
            code_hash,
            confidence: AggregatorLayoutConfidence::TypeAndVersion,
        }
    }

    /// Build evidence from a code-hash cache hit.
    pub fn from_code_hash_cache(
        aggregator: Address,
        code_hash: B256,
        layout: AggregatorLayout,
    ) -> Self {
        Self {
            aggregator,
            type_and_version: None,
            code_hash: Some(code_hash),
            layout,
            confidence: AggregatorLayoutConfidence::CodeHashCache,
        }
    }

    /// Build unknown evidence for an aggregator.
    pub fn unknown(aggregator: Address, code_hash: Option<B256>) -> Self {
        Self {
            aggregator,
            type_and_version: None,
            code_hash,
            layout: AggregatorLayout::Unknown,
            confidence: AggregatorLayoutConfidence::Unknown,
        }
    }

    /// Return true when the evidence supports OCR2 direct storage sync.
    pub fn is_chainlink_ocr2_v1(&self) -> bool {
        self.layout == AggregatorLayout::ChainlinkOcr2V1
    }

    /// Return true when the evidence supports a Chainlink OCR1 direct-storage layout.
    pub fn is_chainlink_ocr1(&self) -> bool {
        matches!(
            self.layout,
            AggregatorLayout::ChainlinkOcr1V2
                | AggregatorLayout::ChainlinkOcr1V3
                | AggregatorLayout::ChainlinkOcr1V4
        )
    }
}

/// Classify a Chainlink-style `typeAndVersion()` string.
pub fn classify_type_and_version(type_and_version: &str) -> AggregatorLayout {
    match type_and_version.trim() {
        "AccessControlledOCR2Aggregator 1.0.0" | "OCR2Aggregator 1.0.0" => {
            AggregatorLayout::ChainlinkOcr2V1
        }
        "AccessControlledOffchainAggregator 2.0.0" | "OffchainAggregator 2.0.0" => {
            AggregatorLayout::ChainlinkOcr1V2
        }
        "AccessControlledOffchainAggregator 3.0.0" | "OffchainAggregator 3.0.0" => {
            AggregatorLayout::ChainlinkOcr1V3
        }
        "AccessControlledOffchainAggregator 4.0.0" | "OffchainAggregator 4.0.0" => {
            AggregatorLayout::ChainlinkOcr1V4
        }
        "" => AggregatorLayout::Unknown,
        _ => AggregatorLayout::Unsupported,
    }
}
