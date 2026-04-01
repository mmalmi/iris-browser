use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintError {
    InvalidAmount,
    ChannelAlreadyExists,
    ChannelMissing,
    ChannelClosed,
    InsufficientBalance {
        available_sat: u64,
        required_sat: u64,
    },
}

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MintError::InvalidAmount => write!(f, "invalid amount"),
            MintError::ChannelAlreadyExists => write!(f, "channel already exists"),
            MintError::ChannelMissing => write!(f, "channel missing"),
            MintError::ChannelClosed => write!(f, "channel closed"),
            MintError::InsufficientBalance {
                available_sat,
                required_sat,
            } => write!(
                f,
                "insufficient channel balance: available={available_sat} required={required_sat}"
            ),
        }
    }
}

impl std::error::Error for MintError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelState {
    pub capacity_sat: u64,
    pub spent_sat: u64,
    pub remaining_sat: u64,
    pub transfer_count: u64,
    pub settled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSettlement {
    pub payer: String,
    pub payee: String,
    pub capacity_sat: u64,
    pub spent_sat: u64,
    pub remaining_sat: u64,
    pub transfer_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ChannelKey {
    payer: String,
    payee: String,
}

#[derive(Debug, Clone)]
struct Channel {
    capacity_sat: u64,
    spent_sat: u64,
    transfer_count: u64,
    settled: bool,
}

#[derive(Debug, Clone, Default)]
pub struct MintStats {
    pub channels_opened: u64,
    pub payments_sent: u64,
    pub payments_failed: u64,
    pub volume_sat: u64,
    pub settlements_finalized: u64,
}

#[derive(Debug, Clone, Default)]
pub struct LocalTestCashuMint {
    channels: HashMap<ChannelKey, Channel>,
    stats: MintStats,
}

impl LocalTestCashuMint {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> MintStats {
        self.stats.clone()
    }

    pub fn open_channel(
        &mut self,
        payer: impl Into<String>,
        payee: impl Into<String>,
        capacity_sat: u64,
    ) -> Result<(), MintError> {
        if capacity_sat == 0 {
            return Err(MintError::InvalidAmount);
        }
        let key = ChannelKey {
            payer: payer.into(),
            payee: payee.into(),
        };
        if self.channels.contains_key(&key) {
            return Err(MintError::ChannelAlreadyExists);
        }
        self.channels.insert(
            key,
            Channel {
                capacity_sat,
                spent_sat: 0,
                transfer_count: 0,
                settled: false,
            },
        );
        self.stats.channels_opened += 1;
        Ok(())
    }

    pub fn transfer(&mut self, payer: &str, payee: &str, amount_sat: u64) -> Result<(), MintError> {
        if amount_sat == 0 {
            return Err(MintError::InvalidAmount);
        }
        let key = ChannelKey {
            payer: payer.to_string(),
            payee: payee.to_string(),
        };
        let Some(channel) = self.channels.get_mut(&key) else {
            self.stats.payments_failed += 1;
            return Err(MintError::ChannelMissing);
        };
        if channel.settled {
            self.stats.payments_failed += 1;
            return Err(MintError::ChannelClosed);
        }
        let available_sat = channel.capacity_sat.saturating_sub(channel.spent_sat);
        if amount_sat > available_sat {
            self.stats.payments_failed += 1;
            return Err(MintError::InsufficientBalance {
                available_sat,
                required_sat: amount_sat,
            });
        }
        channel.spent_sat += amount_sat;
        channel.transfer_count += 1;
        self.stats.payments_sent += 1;
        self.stats.volume_sat += amount_sat;
        Ok(())
    }

    pub fn channel_state(&self, payer: &str, payee: &str) -> Option<ChannelState> {
        let key = ChannelKey {
            payer: payer.to_string(),
            payee: payee.to_string(),
        };
        self.channels.get(&key).map(|channel| ChannelState {
            capacity_sat: channel.capacity_sat,
            spent_sat: channel.spent_sat,
            remaining_sat: channel.capacity_sat.saturating_sub(channel.spent_sat),
            transfer_count: channel.transfer_count,
            settled: channel.settled,
        })
    }

    pub fn settle_channel(
        &mut self,
        payer: &str,
        payee: &str,
    ) -> Result<ChannelSettlement, MintError> {
        let key = ChannelKey {
            payer: payer.to_string(),
            payee: payee.to_string(),
        };
        let Some(channel) = self.channels.get_mut(&key) else {
            return Err(MintError::ChannelMissing);
        };
        if !channel.settled {
            channel.settled = true;
            self.stats.settlements_finalized += 1;
        }
        Ok(ChannelSettlement {
            payer: key.payer,
            payee: key.payee,
            capacity_sat: channel.capacity_sat,
            spent_sat: channel.spent_sat,
            remaining_sat: channel.capacity_sat.saturating_sub(channel.spent_sat),
            transfer_count: channel.transfer_count,
        })
    }

    pub fn settle_all(&mut self) -> Vec<ChannelSettlement> {
        let keys: Vec<(String, String)> = self
            .channels
            .keys()
            .map(|k| (k.payer.clone(), k.payee.clone()))
            .collect();
        let mut settlements = Vec::with_capacity(keys.len());
        for (payer, payee) in keys {
            if let Ok(settlement) = self.settle_channel(&payer, &payee) {
                settlements.push(settlement);
            }
        }
        settlements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_transfers_and_settles_channel() {
        let mut mint = LocalTestCashuMint::new();
        mint.open_channel("alice", "bob", 100)
            .expect("open channel");
        mint.transfer("alice", "bob", 30).expect("payment");
        mint.transfer("alice", "bob", 40).expect("payment");

        let before = mint.channel_state("alice", "bob").expect("state");
        assert_eq!(before.spent_sat, 70);
        assert_eq!(before.remaining_sat, 30);
        assert_eq!(before.transfer_count, 2);
        assert!(!before.settled);

        let settled = mint.settle_channel("alice", "bob").expect("settle");
        assert_eq!(settled.spent_sat, 70);
        assert_eq!(settled.remaining_sat, 30);
        assert_eq!(settled.transfer_count, 2);

        let after = mint.channel_state("alice", "bob").expect("state");
        assert!(after.settled);
    }

    #[test]
    fn allows_many_offchain_transfers_until_balance_runs_out() {
        let mut mint = LocalTestCashuMint::new();
        mint.open_channel("alice", "relay", 1_000)
            .expect("open channel");

        for _ in 0..1_000 {
            mint.transfer("alice", "relay", 1).expect("micropayment");
        }

        let err = mint.transfer("alice", "relay", 1).expect_err("must fail");
        assert_eq!(
            err,
            MintError::InsufficientBalance {
                available_sat: 0,
                required_sat: 1
            }
        );

        let state = mint.channel_state("alice", "relay").expect("state");
        assert_eq!(state.transfer_count, 1_000);
        assert_eq!(state.remaining_sat, 0);
    }

    #[test]
    fn settle_all_is_idempotent_for_stats() {
        let mut mint = LocalTestCashuMint::new();
        mint.open_channel("a", "b", 10).expect("open channel");
        mint.open_channel("a", "c", 10).expect("open channel");
        mint.transfer("a", "b", 2).expect("payment");

        let first = mint.settle_all();
        assert_eq!(first.len(), 2);
        assert_eq!(mint.stats().settlements_finalized, 2);

        let second = mint.settle_all();
        assert_eq!(second.len(), 2);
        assert_eq!(mint.stats().settlements_finalized, 2);
    }
}
