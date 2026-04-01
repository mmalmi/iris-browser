use anyhow::Result;
use async_trait::async_trait;
use nostr::Event;
use std::sync::Arc;
use std::time::Duration;

use super::root_events::PeerRootEvent;

#[async_trait]
pub trait LocalNostrBus: Send + Sync {
    fn source_name(&self) -> &'static str;

    async fn broadcast_event(&self, event: &Event) -> Result<()>;

    async fn query_root(
        &self,
        owner_pubkey: &str,
        tree_name: &str,
        timeout: Duration,
    ) -> Option<PeerRootEvent>;
}

pub type SharedLocalNostrBus = Arc<dyn LocalNostrBus>;
