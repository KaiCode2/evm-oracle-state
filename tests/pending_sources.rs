#![cfg(feature = "pending-oracle-updates")]

use evm_oracle_state::{PendingOracleCandidateSource, PendingOracleSource, PendingOracleSourceId};

#[cfg(feature = "pending-oracle-ethereum")]
#[test]
fn alchemy_source_is_public_mempool_and_redacts_its_url() {
    let source = evm_oracle_state::AlchemyPendingTransactionSource::new(
        "wss://eth-mainnet.g.alchemy.com/v2/secret-key",
    )
    .source_id(PendingOracleSourceId::new("alchemy-primary"));

    let descriptor = source.descriptor();
    assert_eq!(descriptor.source, PendingOracleSource::PublicMempool);
    assert_eq!(descriptor.id, PendingOracleSourceId::new("alchemy-primary"));
    assert!(!format!("{source:?}").contains("secret-key"));
}

#[cfg(feature = "pending-oracle-mev-share")]
#[test]
fn mev_share_source_uses_hash_bound_transport_and_redacts_its_url() {
    let source = evm_oracle_state::MevSharePendingTransactionSource::new(
        "https://mev-share.flashbots.net?token=secret",
    )
    .source_id(PendingOracleSourceId::new("mev-share-primary"));

    let descriptor = source.descriptor();
    assert_eq!(descriptor.source, PendingOracleSource::MevShare);
    assert_eq!(
        descriptor.id,
        PendingOracleSourceId::new("mev-share-primary")
    );
    assert!(!format!("{source:?}").contains("secret"));
}
