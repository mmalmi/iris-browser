use anyhow::{bail, Context, Result};
use cdk::mint_url::MintUrl;
use cdk::nuts::{CurrencyUnit, PaymentMethod, Token};
use cdk::wallet::{ReceiveOptions, SendOptions, WalletRepository, WalletRepositoryBuilder};
use cdk::Amount;
use cdk_sqlite::WalletSqliteDatabase;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use url::Url;

use crate::helper::{CashuMintBalance, CashuReceivedPayment, CashuSentPayment};

pub const CASHU_WALLET_SEED_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CashuWalletSeedFile {
    pub version: u32,
    pub seed_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashuWalletEntry {
    pub mint_url: String,
    pub unit: String,
    pub balance: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashuUnitTotal {
    pub unit: String,
    pub balance: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CashuWalletOverview {
    pub totals: Vec<CashuUnitTotal>,
    pub entries: Vec<CashuWalletEntry>,
    pub warnings: Vec<String>,
    pub legacy_state_detected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashuTopupQuote {
    pub mint_url: String,
    pub unit: String,
    pub amount: u64,
    pub quote_id: String,
    pub payment_request: String,
    pub expiry_unix: u64,
}

pub fn cashu_wallet_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("cashu")
}

pub fn cashu_wallet_db_path(data_dir: &Path) -> PathBuf {
    cashu_wallet_dir(data_dir).join("wallet.sqlite")
}

pub fn cashu_wallet_seed_path(data_dir: &Path) -> PathBuf {
    cashu_wallet_dir(data_dir).join("seed.json")
}

pub fn legacy_cashu_wallet_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("cashu-wallet.json")
}

pub fn normalize_mint_url(raw: &str) -> Result<String> {
    let mut url = Url::parse(raw).with_context(|| format!("Invalid mint URL: {raw}"))?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => bail!("Unsupported mint URL scheme: {scheme}"),
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("Mint URL must not include query or fragment");
    }

    let trimmed_path = url.path().trim_end_matches('/').to_string();
    if trimmed_path.is_empty() {
        url.set_path("");
    } else {
        url.set_path(&trimmed_path);
    }

    Ok(url.to_string().trim_end_matches('/').to_string())
}

pub fn load_or_create_wallet_seed(path: &Path) -> Result<[u8; 64]> {
    if path.exists() {
        let content = fs::read_to_string(path).context("Failed to read Cashu wallet seed")?;
        let seed_file: CashuWalletSeedFile =
            serde_json::from_str(&content).context("Failed to parse Cashu wallet seed")?;
        let seed_bytes = hex::decode(seed_file.seed_hex).context("Invalid Cashu wallet seed")?;
        let seed: [u8; 64] = seed_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Cashu wallet seed must be 64 bytes"))?;
        return Ok(seed);
    }

    let mut seed = [0_u8; 64];
    rand::thread_rng().fill_bytes(&mut seed);
    write_wallet_seed(path, &seed)?;
    Ok(seed)
}

pub async fn open_wallet_repository(data_dir: &Path) -> Result<WalletRepository> {
    fs::create_dir_all(cashu_wallet_dir(data_dir))
        .context("Failed to create Cashu wallet directory")?;
    let seed = load_or_create_wallet_seed(&cashu_wallet_seed_path(data_dir))?;
    let localstore = Arc::new(
        WalletSqliteDatabase::new(cashu_wallet_db_path(data_dir))
            .await
            .context("Failed to open Cashu wallet database")?,
    );
    let repository = WalletRepositoryBuilder::new()
        .localstore(localstore)
        .seed(seed)
        .build()
        .await
        .context("Failed to build Cashu wallet repository")?;
    Ok(repository)
}

pub async fn load_wallet_overview(
    data_dir: &Path,
    refresh_quotes: bool,
) -> Result<CashuWalletOverview> {
    let repository = open_wallet_repository(data_dir).await?;
    let mut warnings = Vec::new();

    if refresh_quotes {
        for wallet in repository.get_wallets().await {
            let mint_label = format!("{} ({})", wallet.mint_url, wallet.unit);
            if let Err(err) = wallet.recover_incomplete_sagas().await {
                warnings.push(format!(
                    "Failed to recover wallet state for {mint_label}: {err}"
                ));
                continue;
            }
            if let Err(err) = wallet.mint_unissued_quotes().await {
                warnings.push(format!(
                    "Failed to refresh pending mint quotes for {mint_label}: {err}"
                ));
            }
        }
    }

    let totals = repository
        .total_balance()
        .await
        .context("Failed to load Cashu wallet totals")?
        .into_iter()
        .map(|(unit, amount)| CashuUnitTotal {
            unit: unit.to_string(),
            balance: amount.to_u64(),
        })
        .collect();

    let entries = repository
        .get_balances()
        .await
        .context("Failed to load Cashu wallet balances")?
        .into_iter()
        .map(|(key, amount)| CashuWalletEntry {
            mint_url: key.mint_url.to_string(),
            unit: key.unit.to_string(),
            balance: amount.to_u64(),
        })
        .collect();

    Ok(CashuWalletOverview {
        totals,
        entries,
        warnings,
        legacy_state_detected: legacy_cashu_wallet_state_path(data_dir).exists(),
    })
}

pub async fn create_topup_quote(
    data_dir: &Path,
    mint_url: &str,
    amount_sat: u64,
) -> Result<CashuTopupQuote> {
    if amount_sat == 0 {
        bail!("Cashu topup amount must be greater than zero");
    }

    let normalized_mint = normalize_mint_url(mint_url)?;
    let mint_url =
        MintUrl::from_str(&normalized_mint).context("Failed to parse normalized mint URL")?;
    let repository = open_wallet_repository(data_dir).await?;
    let wallet = ensure_sat_wallet(&repository, &mint_url).await?;

    wallet
        .recover_incomplete_sagas()
        .await
        .context("Failed to recover Cashu wallet state before creating quote")?;
    wallet
        .mint_unissued_quotes()
        .await
        .context("Failed to refresh pending Cashu mint quotes before creating quote")?;

    let quote = wallet
        .mint_quote(
            PaymentMethod::BOLT11,
            Some(Amount::from(amount_sat)),
            None,
            None,
        )
        .await
        .context("Failed to create Cashu mint quote")?;

    Ok(CashuTopupQuote {
        mint_url: normalized_mint,
        unit: CurrencyUnit::Sat.to_string(),
        amount: amount_sat,
        quote_id: quote.id,
        payment_request: quote.request,
        expiry_unix: quote.expiry,
    })
}

pub async fn load_mint_balance(data_dir: &Path, mint_url: &str) -> Result<CashuMintBalance> {
    let normalized_mint = normalize_mint_url(mint_url)?;
    let mint_url =
        MintUrl::from_str(&normalized_mint).context("Failed to parse normalized mint URL")?;
    let repository = open_wallet_repository(data_dir).await?;
    ensure_sat_wallet(&repository, &mint_url).await?;

    let balance_sat = repository
        .get_balances()
        .await
        .context("Failed to load Cashu wallet balances")?
        .into_iter()
        .find_map(|(key, amount)| {
            (key.mint_url == mint_url && key.unit == CurrencyUnit::Sat).then_some(amount.to_u64())
        })
        .unwrap_or_default();

    Ok(CashuMintBalance {
        mint_url: normalized_mint,
        unit: CurrencyUnit::Sat.to_string(),
        balance_sat,
    })
}

pub async fn send_payment_token(
    data_dir: &Path,
    mint_url: &str,
    amount_sat: u64,
) -> Result<CashuSentPayment> {
    if amount_sat == 0 {
        bail!("Cashu payment amount must be greater than zero");
    }

    let normalized_mint = normalize_mint_url(mint_url)?;
    let mint_url =
        MintUrl::from_str(&normalized_mint).context("Failed to parse normalized mint URL")?;
    let repository = open_wallet_repository(data_dir).await?;
    let wallet = ensure_sat_wallet(&repository, &mint_url).await?;

    wallet
        .recover_incomplete_sagas()
        .await
        .context("Failed to recover Cashu wallet state before sending payment")?;

    let prepared = wallet
        .prepare_send(
            Amount::from(amount_sat),
            SendOptions {
                include_fee: true,
                ..Default::default()
            },
        )
        .await
        .context("Failed to prepare Cashu payment token")?;
    let operation_id = prepared.operation_id().to_string();
    let send_fee_sat = prepared.send_fee().to_u64();
    let token = prepared
        .confirm(None)
        .await
        .context("Failed to create Cashu payment token")?;

    Ok(CashuSentPayment {
        mint_url: normalized_mint,
        unit: CurrencyUnit::Sat.to_string(),
        amount_sat,
        send_fee_sat,
        operation_id,
        token: token.to_string(),
    })
}

pub async fn receive_payment_token(
    data_dir: &Path,
    encoded_token: &str,
) -> Result<CashuReceivedPayment> {
    let token = Token::from_str(encoded_token).context("Failed to parse Cashu token")?;
    let mint_url = token
        .mint_url()
        .context("Cashu token must contain exactly one mint")?;
    let unit = token.unit().unwrap_or_default();
    if unit != CurrencyUnit::Sat {
        bail!("Unsupported Cashu token unit: {unit}");
    }
    let normalized_mint = normalize_mint_url(&mint_url.to_string())?;

    let repository = open_wallet_repository(data_dir).await?;
    let wallet = ensure_sat_wallet(&repository, &mint_url).await?;
    wallet
        .recover_incomplete_sagas()
        .await
        .context("Failed to recover Cashu wallet state before receiving payment")?;

    let amount_received = wallet
        .receive(encoded_token, ReceiveOptions::default())
        .await
        .context("Failed to receive Cashu payment token")?;

    Ok(CashuReceivedPayment {
        mint_url: normalized_mint,
        unit: CurrencyUnit::Sat.to_string(),
        amount_sat: amount_received.to_u64(),
    })
}

pub async fn revoke_pending_payment(
    data_dir: &Path,
    mint_url: &str,
    operation_id: &str,
) -> Result<u64> {
    let normalized_mint = normalize_mint_url(mint_url)?;
    let mint_url =
        MintUrl::from_str(&normalized_mint).context("Failed to parse normalized mint URL")?;
    let repository = open_wallet_repository(data_dir).await?;
    let wallet = ensure_sat_wallet(&repository, &mint_url).await?;
    wallet
        .recover_incomplete_sagas()
        .await
        .context("Failed to recover Cashu wallet state before revoking payment")?;

    let operation_id = operation_id
        .parse()
        .context("Invalid Cashu send operation id")?;
    let amount = wallet
        .revoke_send(operation_id)
        .await
        .context("Failed to revoke Cashu payment token")?;
    Ok(amount.to_u64())
}

fn write_wallet_seed(path: &Path, seed: &[u8; 64]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create Cashu wallet directory")?;
    }

    let seed_file = CashuWalletSeedFile {
        version: CASHU_WALLET_SEED_VERSION,
        seed_hex: hex::encode(seed),
    };
    let content =
        serde_json::to_string_pretty(&seed_file).context("Failed to encode Cashu wallet seed")?;
    fs::write(path, content).context("Failed to write Cashu wallet seed")?;
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .context("Failed to secure Cashu wallet seed permissions")?;
    }
    Ok(())
}

async fn ensure_sat_wallet(
    repository: &WalletRepository,
    mint_url: &MintUrl,
) -> Result<cdk::wallet::Wallet> {
    let wallet = if repository.has_wallet(mint_url, &CurrencyUnit::Sat).await {
        repository
            .get_wallet(mint_url, &CurrencyUnit::Sat)
            .await
            .context("Failed to load existing Cashu sat wallet")?
    } else {
        repository
            .create_wallet(mint_url.clone(), CurrencyUnit::Sat, None)
            .await
            .context("Failed to create Cashu sat wallet")?
    };

    wallet
        .localstore
        .add_mint(mint_url.clone(), None)
        .await
        .context("Failed to persist Cashu mint metadata")?;

    Ok(wallet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_mint_url_trims_trailing_slash_and_rejects_query() {
        assert_eq!(
            normalize_mint_url("https://mint.example/").unwrap(),
            "https://mint.example"
        );
        assert_eq!(
            normalize_mint_url("http://127.0.0.1:3338/api/v1/").unwrap(),
            "http://127.0.0.1:3338/api/v1"
        );
        assert!(normalize_mint_url("wss://mint.example").is_err());
        assert!(normalize_mint_url("https://mint.example/?x=1").is_err());
    }

    #[test]
    fn test_cashu_wallet_seed_roundtrip_and_paths() {
        let temp_dir = tempfile::tempdir().unwrap();
        let seed_path = cashu_wallet_seed_path(temp_dir.path());
        let db_path = cashu_wallet_db_path(temp_dir.path());
        assert_eq!(seed_path, temp_dir.path().join("cashu").join("seed.json"));
        assert_eq!(db_path, temp_dir.path().join("cashu").join("wallet.sqlite"));

        let seed = load_or_create_wallet_seed(&seed_path).unwrap();
        assert_eq!(seed.len(), 64);
        let restored = load_or_create_wallet_seed(&seed_path).unwrap();
        assert_eq!(restored, seed);
    }

    #[tokio::test]
    async fn test_wallet_overview_loads_stored_wallets() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = open_wallet_repository(temp_dir.path()).await.unwrap();
        let mint_url: MintUrl = "https://mint.example".parse().unwrap();
        ensure_sat_wallet(&repo, &mint_url).await.unwrap();

        let overview = load_wallet_overview(temp_dir.path(), false).await.unwrap();
        assert_eq!(
            overview.entries,
            vec![CashuWalletEntry {
                mint_url: "https://mint.example".to_string(),
                unit: "sat".to_string(),
                balance: 0,
            }]
        );
        assert_eq!(
            overview.totals,
            vec![CashuUnitTotal {
                unit: "sat".to_string(),
                balance: 0,
            }]
        );
    }

    #[tokio::test]
    async fn test_load_mint_balance_returns_zero_for_known_wallet() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo = open_wallet_repository(temp_dir.path()).await.unwrap();
        let mint_url: MintUrl = "https://mint.example".parse().unwrap();
        ensure_sat_wallet(&repo, &mint_url).await.unwrap();

        let balance = load_mint_balance(temp_dir.path(), "https://mint.example")
            .await
            .unwrap();
        assert_eq!(balance.mint_url, "https://mint.example");
        assert_eq!(balance.unit, "sat");
        assert_eq!(balance.balance_sat, 0);
    }

    #[tokio::test]
    async fn test_create_topup_quote_rejects_zero_amount() {
        let temp_dir = tempfile::tempdir().unwrap();
        let err = create_topup_quote(temp_dir.path(), "https://mint.example", 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("greater than zero"));
    }

    #[tokio::test]
    async fn test_send_payment_token_rejects_zero_amount() {
        let temp_dir = tempfile::tempdir().unwrap();
        let err = send_payment_token(temp_dir.path(), "https://mint.example", 0)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("greater than zero"));
    }

    #[tokio::test]
    async fn test_receive_payment_token_rejects_invalid_token() {
        let temp_dir = tempfile::tempdir().unwrap();
        let err = receive_payment_token(temp_dir.path(), "not-a-token")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("parse Cashu token"));
    }

    #[tokio::test]
    async fn test_revoke_pending_payment_rejects_invalid_operation_id() {
        let temp_dir = tempfile::tempdir().unwrap();
        let err = revoke_pending_payment(temp_dir.path(), "https://mint.example", "nope")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Invalid Cashu send operation id"));
    }

    #[tokio::test]
    async fn test_create_topup_quote_against_configured_mint() {
        let mint_url = match std::env::var("HTREE_CASHU_TEST_MINT_URL") {
            Ok(value) => value,
            Err(_) => return,
        };

        let temp_dir = tempfile::tempdir().unwrap();
        let quote = create_topup_quote(temp_dir.path(), &mint_url, 1)
            .await
            .unwrap();
        assert_eq!(quote.amount, 1);
        assert_eq!(quote.unit, "sat");
        assert!(!quote.quote_id.is_empty());
        assert!(!quote.payment_request.is_empty());
    }
}
