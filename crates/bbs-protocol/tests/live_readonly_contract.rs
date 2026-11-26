//! Opt-in single-shot read-only live contract test for `bbs-protocol`.
//!
//! Runs ONLY when BOTH:
//! - invoked explicitly with `--ignored`, AND
//! - `BBSCORD_LIVE_TDD=1` is set in the environment.
//!
//! Security guarantees:
//! - Reads OS keyring strictly in RAM (`bbscord` primary, or read-only `discordo` fallback).
//! - NEVER prints, asserts-with-value, logs, or saves any token, user ID, username, email, or guild data.
//! - Drops the zeroizing [`SecretToken`] immediately after the single passive `GET /api/v10/users/@me` check.

use bbs_core::{KeyringStore, OsKeyringStore, SecretToken};
use bbs_protocol::{DiscordFingerprint, DiscordRestClient};

fn load_ram_token_readonly() -> Option<SecretToken> {
    let store = OsKeyringStore::new();
    match store.load_bbscord_token() {
        Ok(Some(token)) => return Some(token),
        Ok(None) => {}
        Err(_) => {
            // Keyring locked or unavailable; fall through to check discordo or skip cleanly.
        }
    }

    match store.import_discordo_token_ram_only() {
        Ok(Some(token)) => Some(token),
        Ok(None) | Err(_) => None,
    }
}

#[tokio::test]
#[ignore = "opt-in live read-only contract test; requires BBSCORD_LIVE_TDD=1 and --ignored"]
async fn live_readonly_users_me_contract() {
    if std::env::var("BBSCORD_LIVE_TDD").as_deref() != Ok("1") {
        eprintln!("[live_readonly_contract] SKIP: BBSCORD_LIVE_TDD!=1");
        return;
    }

    let Some(token) = load_ram_token_readonly() else {
        eprintln!("[live_readonly_contract] SKIP: no unlocked keyring token available");
        return;
    };

    let fingerprint = match DiscordFingerprint::default_chrome() {
        Ok(fp) => fp,
        Err(_) => {
            drop(token);
            panic!("failed to construct Chrome TLS/HTTP2 fingerprint");
        }
    };

    let client = match DiscordRestClient::new(token, fingerprint) {
        Ok(c) => c,
        Err(_) => {
            panic!("failed to construct DiscordRestClient");
        }
    };

    let check = client.check_token_contract_readonly().await;
    // Drop client immediately so the internal zeroizing SecretToken is scrubbed from RAM.
    drop(client);

    match check {
        Ok(result) => {
            assert_eq!(
                result.status_code, 200,
                "GET /api/v10/users/@me returned non-200 status code"
            );
            assert!(
                result.schema_valid,
                "GET /api/v10/users/@me JSON schema validation failed"
            );
            eprintln!("[live_readonly_contract] PASS: HTTP 200 schema verified");
        }
        Err(err) => {
            panic!("GET /api/v10/users/@me contract check failed: {err}");
        }
    }
}
