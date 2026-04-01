use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::cashu_test_mint::{
    ChannelSettlement, ChannelState, LocalTestCashuMint, MintError, MintStats,
};

#[async_trait]
pub trait MintClient: Send + Sync {
    async fn open_channel(
        &self,
        payer: &str,
        payee: &str,
        capacity_sat: u64,
    ) -> Result<(), MintError>;

    async fn transfer(&self, payer: &str, payee: &str, amount_sat: u64) -> Result<(), MintError>;

    async fn channel_state(
        &self,
        payer: &str,
        payee: &str,
    ) -> Result<Option<ChannelState>, MintError>;

    async fn settle_all(&self) -> Result<Vec<ChannelSettlement>, MintError>;

    async fn stats(&self) -> Result<MintStats, MintError>;
}

#[derive(Debug, Clone, Default)]
pub struct LocalMintClient {
    inner: Arc<RwLock<LocalTestCashuMint>>,
}

impl LocalMintClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_inner(inner: Arc<RwLock<LocalTestCashuMint>>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> Arc<RwLock<LocalTestCashuMint>> {
        self.inner.clone()
    }
}

#[async_trait]
impl MintClient for LocalMintClient {
    async fn open_channel(
        &self,
        payer: &str,
        payee: &str,
        capacity_sat: u64,
    ) -> Result<(), MintError> {
        self.inner
            .write()
            .await
            .open_channel(payer, payee, capacity_sat)
    }

    async fn transfer(&self, payer: &str, payee: &str, amount_sat: u64) -> Result<(), MintError> {
        self.inner.write().await.transfer(payer, payee, amount_sat)
    }

    async fn channel_state(
        &self,
        payer: &str,
        payee: &str,
    ) -> Result<Option<ChannelState>, MintError> {
        Ok(self.inner.read().await.channel_state(payer, payee))
    }

    async fn settle_all(&self) -> Result<Vec<ChannelSettlement>, MintError> {
        Ok(self.inner.write().await.settle_all())
    }

    async fn stats(&self) -> Result<MintStats, MintError> {
        Ok(self.inner.read().await.stats())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_mint_client_wraps_in_process_mint() {
        let client = LocalMintClient::new();
        client
            .open_channel("alice", "bob", 100)
            .await
            .expect("open channel");
        client.transfer("alice", "bob", 30).await.expect("payment");

        let state = client
            .channel_state("alice", "bob")
            .await
            .expect("channel state")
            .expect("channel must exist");
        assert_eq!(state.remaining_sat, 70);

        let settlements = client.settle_all().await.expect("settle all");
        assert_eq!(settlements.len(), 1);

        let stats = client.stats().await.expect("mint stats");
        assert_eq!(stats.channels_opened, 1);
        assert_eq!(stats.payments_sent, 1);
        assert_eq!(stats.volume_sat, 30);
        assert_eq!(stats.settlements_finalized, 1);
    }
}
