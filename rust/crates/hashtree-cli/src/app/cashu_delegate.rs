use anyhow::{Context, Result};
use hashtree_cli::cashu_helper::{base_helper_args, helper_binary_path, run_helper_status};
use std::ffi::OsString;
use std::path::Path;

use super::args::{CashuCommands, CashuMintCommands};

pub(crate) fn run_cashu_helper(data_dir: &Path, command: &CashuCommands) -> Result<()> {
    let current_exe =
        std::env::current_exe().context("Failed to determine htree executable path")?;
    let helper = helper_binary_path(&current_exe)?;
    let args = build_cashu_helper_args(data_dir, command);
    run_helper_status(&helper, &args)
}

fn build_cashu_helper_args(data_dir: &Path, command: &CashuCommands) -> Vec<OsString> {
    let mut args = base_helper_args(data_dir).to_vec();

    match command {
        CashuCommands::Balance { mint } => {
            args.push(OsString::from("balance"));
            if let Some(mint) = mint {
                args.push(OsString::from("--mint"));
                args.push(OsString::from(mint));
            }
        }
        CashuCommands::Topup { amount_sat, mint } => {
            args.push(OsString::from("topup"));
            args.push(OsString::from(amount_sat.to_string()));
            if let Some(mint) = mint {
                args.push(OsString::from("--mint"));
                args.push(OsString::from(mint));
            }
        }
        CashuCommands::Mint { command } => {
            args.push(OsString::from("mint"));
            match command {
                CashuMintCommands::List => {
                    args.push(OsString::from("list"));
                }
                CashuMintCommands::Add { url, make_default } => {
                    args.push(OsString::from("add"));
                    args.push(OsString::from(url));
                    if *make_default {
                        args.push(OsString::from("--default"));
                    }
                }
                CashuMintCommands::Remove { url } => {
                    args.push(OsString::from("remove"));
                    args.push(OsString::from(url));
                }
                CashuMintCommands::Default { url } => {
                    args.push(OsString::from("default"));
                    args.push(OsString::from(url));
                }
            }
        }
    }

    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn test_build_cashu_helper_args_for_balance_and_mint_commands() {
        let data_dir = Path::new("/tmp/htree-data");

        let args = build_cashu_helper_args(
            data_dir,
            &CashuCommands::Balance {
                mint: Some("https://mint.example".to_string()),
            },
        );
        assert_eq!(
            args,
            vec![
                OsString::from("--data-dir"),
                OsString::from("/tmp/htree-data"),
                OsString::from("balance"),
                OsString::from("--mint"),
                OsString::from("https://mint.example"),
            ]
        );

        let args = build_cashu_helper_args(
            data_dir,
            &CashuCommands::Mint {
                command: CashuMintCommands::Add {
                    url: "https://mint.example".to_string(),
                    make_default: true,
                },
            },
        );
        assert_eq!(
            args,
            vec![
                OsString::from("--data-dir"),
                OsString::from("/tmp/htree-data"),
                OsString::from("mint"),
                OsString::from("add"),
                OsString::from("https://mint.example"),
                OsString::from("--default"),
            ]
        );
    }

    #[test]
    fn test_helper_binary_path_prefers_env_override() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        let temp_dir = tempfile::tempdir().unwrap();
        let override_path = temp_dir.path().join("custom-helper");
        std::fs::write(&override_path, b"").unwrap();

        env::set_var(hashtree_cli::cashu_helper::CASHU_HELPER_ENV, &override_path);
        env::remove_var(hashtree_cli::cashu_helper::CARGO_HELPER_ENV);

        let resolved = helper_binary_path(Path::new("/tmp/htree")).unwrap();
        assert_eq!(resolved, override_path);

        env::remove_var(hashtree_cli::cashu_helper::CASHU_HELPER_ENV);
    }

    #[test]
    fn test_helper_binary_path_falls_back_to_sibling_binary() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        env::remove_var(hashtree_cli::cashu_helper::CASHU_HELPER_ENV);
        env::remove_var(hashtree_cli::cashu_helper::CARGO_HELPER_ENV);

        let temp_dir = tempfile::tempdir().unwrap();
        let current_exe = temp_dir.path().join("htree");
        std::fs::write(&current_exe, b"").unwrap();
        let sibling = temp_dir
            .path()
            .join(hashtree_cli::cashu_helper::helper_binary_name());
        std::fs::write(&sibling, b"").unwrap();

        let resolved = helper_binary_path(&current_exe).unwrap();
        assert_eq!(resolved, sibling);
    }

    #[cfg(unix)]
    #[test]
    fn test_run_cashu_helper_with_env_override_executes_helper() {
        let _guard = env_lock().lock().unwrap_or_else(|err| err.into_inner());
        env::remove_var(hashtree_cli::cashu_helper::CARGO_HELPER_ENV);

        let temp_dir = tempfile::tempdir().unwrap();
        let output_path = temp_dir.path().join("args.txt");
        let helper_path = temp_dir.path().join("htree-cashu-stub");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            output_path.display()
        );
        std::fs::write(&helper_path, script).unwrap();
        let mut perms = std::fs::metadata(&helper_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&helper_path, perms).unwrap();

        env::set_var(hashtree_cli::cashu_helper::CASHU_HELPER_ENV, &helper_path);
        run_cashu_helper(
            Path::new("/tmp/htree-data"),
            &CashuCommands::Topup {
                amount_sat: 42,
                mint: Some("https://mint.example".to_string()),
            },
        )
        .unwrap();

        let args = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(
            args,
            "--data-dir\n/tmp/htree-data\ntopup\n42\n--mint\nhttps://mint.example\n"
        );

        env::remove_var(hashtree_cli::cashu_helper::CASHU_HELPER_ENV);
    }
}
