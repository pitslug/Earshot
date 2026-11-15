//! Setting a device up for sync and removing it (docs/sync-spec.md section 6.3).
//!
//! The setup secret and a pairing code are used for one request and never stored; the vault
//! key and the device token go only to the [`SyncVault`].

use earshot_net::Url;
use earshot_sync::wire::{B64, PairingRequest, RegisterRequest, RevokeRequest, TakeRequest};
use earshot_sync::{PairingCode, VaultKey};

use super::client::Relay;
use super::engine::SyncClient;
use super::{
    DeviceToken, SYNC_CURSOR_KEY, SYNC_ON_KEY, SYNC_SERVER_KEY, SYNC_STATUS_KEY,
    SYNC_VOICEPRINTS_KEY, SyncProblem, SyncVault, check_address, encode_cursor,
};
use crate::local::{self, DeviceRecord};
use crate::services::Services;

/// A pairing code to show, with how long the server keeps it. `Debug` shows no code; the text
/// is wiped on drop. Show it once and drop it.
pub struct PairingOffer {
    code: String,
    /// Seconds until the code stops working.
    pub expires_in_s: u64,
}

impl PairingOffer {
    /// The code as the person types it on the other device (groups of characters).
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }
}

impl Drop for PairingOffer {
    fn drop(&mut self) {
        earshot_net::wipe_string(&mut self.code);
    }
}

impl std::fmt::Debug for PairingOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingOffer")
            .field("expires_in_s", &self.expires_in_s)
            .finish_non_exhaustive()
    }
}

fn device_bytes(services: &Services) -> B64 {
    B64(services.store().device().as_uuid().as_bytes().to_vec())
}

/// Stores what a registered device keeps: key and token in the vault; every synced row queued
/// again and every held row dropped (earlier sends went to no vault or another one, and held
/// rows were sealed under another key); then the address, a pull cursor of 0 and sync on, in
/// one transaction.
fn keep(
    services: &Services,
    vault: &dyn SyncVault,
    address: &Url,
    key: &VaultKey,
    token: &DeviceToken,
) -> Result<(), SyncProblem> {
    vault.save(key, token)?;
    services.store().queue_everything()?;
    crate::vocabulary::with_stamp(services, |tx, stamp| -> Result<(), SyncProblem> {
        local::set_tx(
            tx,
            &DeviceRecord::SyncServer.key(),
            address.as_str().as_bytes(),
        )?;
        local::set_tx(tx, &DeviceRecord::SyncCursor.key(), &encode_cursor(0))?;
        local::set_tx(tx, &DeviceRecord::SyncOn.key(), b"on")?;
        // Dictionary sync is on by default: rows for the words here, the list from the rows.
        crate::vocabulary::syncing_turned_on_tx(tx, stamp)?;
        // A thread of an earlier setup may have written a status after it was stopped.
        tx.delete_local_value(SYNC_STATUS_KEY)?;
        Ok(())
    })
}

/// Sets this device up as the vault's first (or with the server's setup secret): a new vault
/// key, registered with the server's `setup_secret`.
///
/// # Errors
/// [`SyncProblem::Address`]; [`SyncProblem::BadSecret`]; [`SyncProblem::KeyMismatch`] when the
/// server already belongs to another vault (join it with a pairing code); the relay's other
/// problems; [`SyncProblem::Store`] when what was issued cannot be kept.
pub fn set_up_first_device(
    services: &Services,
    vault: &dyn SyncVault,
    address: &str,
    setup_secret: &str,
) -> Result<(), SyncProblem> {
    let address = check_address(address).map_err(|_| SyncProblem::Address)?;
    let key = VaultKey::generate().map_err(|_| SyncProblem::Internal)?;
    let relay = Relay::new(address.clone(), std::sync::Arc::clone(&services.clock))?;
    let token = relay.register(&RegisterRequest {
        device_id: device_bytes(services),
        key_check: B64(key.keys().key_check().to_vec()),
        setup_secret: Some(setup_secret.trim().to_owned()),
        slot: None,
    })?;
    keep(services, vault, &address, &key, &token)
}

/// A pairing code for adding another device: the vault key, wrapped under a key only the code
/// gives, offered in a one-use slot the server keeps for 10 minutes.
///
/// # Errors
/// [`SyncProblem::Vault`] when this device is not set up; the relay's problems.
pub fn offer_pairing_code(
    services: &Services,
    vault: std::sync::Arc<dyn SyncVault>,
) -> Result<PairingOffer, SyncProblem> {
    let secrets = vault.load()?.ok_or(SyncProblem::Vault)?;
    let client = SyncClient::new(services.clone(), vault)?;
    let code = PairingCode::generate().map_err(|_| SyncProblem::Internal)?;
    let wrapped = code.wrap(&secrets.key).map_err(|_| SyncProblem::Internal)?;
    let resp = client.relay().offer_slot(
        &secrets.token,
        &PairingRequest {
            slot: B64(code.slot().to_vec()),
            wrapped: B64(wrapped),
        },
    )?;
    Ok(PairingOffer {
        code: code.to_string(),
        expires_in_s: resp.expires_in_s,
    })
}

/// Joins the vault of the server at `address` with a pairing code shown on a device already in
/// it. Pulls from the start on the next tick.
///
/// # Errors
/// [`SyncProblem::Address`]; [`SyncProblem::BadCode`] for a mistyped code;
/// [`SyncProblem::NoSlot`] for a code the server does not hold (wrong, used or expired);
/// [`SyncProblem::DeviceExists`]; the relay's other problems;
/// [`SyncProblem::Store`] when what was issued cannot be kept.
pub fn join_with_code(
    services: &Services,
    vault: &dyn SyncVault,
    address: &str,
    code: &str,
) -> Result<(), SyncProblem> {
    let address = check_address(address).map_err(|_| SyncProblem::Address)?;
    let code = PairingCode::parse(code).map_err(|_| SyncProblem::BadCode)?;
    let relay = Relay::new(address.clone(), std::sync::Arc::clone(&services.clock))?;
    let slot = B64(code.slot().to_vec());
    let taken = relay.take_slot(&TakeRequest { slot: slot.clone() })?;
    // The slot is named by the code, so a wrap that does not open is not this code's.
    let key = code
        .unwrap_key(&taken.wrapped.0)
        .map_err(|_| SyncProblem::Server)?;
    let token = relay.register(&RegisterRequest {
        device_id: device_bytes(services),
        key_check: B64(key.keys().key_check().to_vec()),
        setup_secret: None,
        slot: Some(slot),
    })?;
    keep(services, vault, &address, &key, &token)
}

/// Removes this device from sync: the server revokes its token, then the key, the token, the
/// address, the cursor, the status and the voiceprint switch are deleted here. Meetings stay. A token the server no
/// longer knows counts as revoked. The host stops its [`super::SyncThread`] first, or a tick in
/// flight writes the cursor and status back.
///
/// # Errors
/// The relay's problems (nothing is deleted then, so it can be tried again);
/// [`SyncProblem::Vault`]; [`SyncProblem::Store`].
pub fn remove_this_device(
    services: &Services,
    vault: std::sync::Arc<dyn SyncVault>,
) -> Result<(), SyncProblem> {
    // With no usable address stored there is no server to tell: only this device forgets.
    let client = match SyncClient::new(services.clone(), std::sync::Arc::clone(&vault)) {
        Ok(c) => Some(c),
        Err(SyncProblem::Address) => None,
        Err(p) => return Err(p),
    };
    if let (Some(secrets), Some(client)) = (vault.load()?, client) {
        match client.relay().revoke(
            &secrets.token,
            &RevokeRequest {
                device_id: device_bytes(services),
            },
        ) {
            Ok(()) | Err(SyncProblem::UnknownDevice) => {}
            Err(p) => return Err(p),
        }
    }
    vault.remove()?;
    let mut db = services.store();
    // Voiceprint sync is cleared too: joining a vault again asks for it afresh (biometric).
    for key in [
        SYNC_SERVER_KEY,
        SYNC_CURSOR_KEY,
        SYNC_STATUS_KEY,
        SYNC_ON_KEY,
        SYNC_VOICEPRINTS_KEY,
        super::OFFER_SUMMARIES_KEY,
    ] {
        db.delete_local_value(key)?;
    }
    Ok(())
}
