//! The target-facing end of drive redirection: the RDPDR channel announcing the drives a
//! [`DriveService`] owns, and the loop step relaying their completions back.

use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_rdpdr::Rdpdr;
use ironrdp_rdpsnd::client::{NoopRdpsndBackend, Rdpsnd};
use ironrdp_session::ActiveStage;
use ironrdp_svc::{SvcMessage, SvcProcessorMessages};
use ironrdp_tokio::FramedWrite as _;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::warn;
use warpgate_core::FileTransferRecord;

use super::Framed;
use crate::drive::{DrivePolicy, DriveService, DriveUpdate, SharedLedger, TargetDrives, ledger};
use crate::drive_uploads::UploadDrive;

/// The name the target shows redirected drives under: "`<drive>` on Warpgate".
const COMPUTER_NAME: &str = "Warpgate";

/// Where the target's drives come from and what it may do with them.
#[derive(Debug, Clone)]
pub(crate) struct DriveSetup {
    pub service: Arc<dyn DriveService>,
    pub policy: DrivePolicy,
    /// Set when the drive is the browser's session drive, which takes uploads.
    pub uploads: Option<UploadDrive>,
}

/// A running drive relay inside the target session.
pub(super) struct DriveLink {
    pub setup: DriveSetup,
    ledger: SharedLedger,
    updates: UnboundedReceiver<DriveUpdate>,
}

impl DriveLink {
    /// The relay, and the static channels the connector must carry for it. RDPDR is
    /// only serviced by Windows when RDPSND is also present ([MS-RDPEFS] Appendix A
    /// <1>), so a silent sound channel comes along.
    pub(super) fn new(setup: DriveSetup) -> (Self, Rdpdr, Rdpsnd) {
        let ledger = SharedLedger::default();
        let (updates_tx, updates) = unbounded_channel();
        let rdpdr = Rdpdr::new(
            Box::new(TargetDrives::new(
                Arc::clone(&setup.service),
                setup.policy,
                Arc::clone(&ledger),
            )),
            COMPUTER_NAME.to_owned(),
        )
        .with_drives(None);
        setup.service.attach(updates_tx);
        (
            Self {
                setup,
                ledger,
                updates,
            },
            rdpdr,
            Rdpsnd::new(Box::new(NoopRdpsndBackend)),
        )
    }

    pub(super) async fn next(&mut self) -> Option<DriveUpdate> {
        self.updates.recv().await
    }

    /// Apply one update from the drive's owner. Returns the transfer records it produced.
    pub(super) async fn apply(
        &mut self,
        update: DriveUpdate,
        framed: &mut Framed,
        active_stage: &mut ActiveStage,
    ) -> Result<Vec<FileTransferRecord>> {
        let mut records = Vec::new();
        let Some(rdpdr) = active_stage.get_svc_processor_mut::<Rdpdr>() else {
            return Ok(records);
        };
        let messages: Vec<SvcMessage> = match update {
            DriveUpdate::Announce(drives) => {
                let mut messages = Vec::new();
                for (device_id, name) in drives {
                    match rdpdr.add_dynamic_drive(device_id, name) {
                        Ok(announce) => messages.extend(announce),
                        Err(error) => {
                            warn!(device_id, %error, "Failed to announce a redirected drive");
                        }
                    }
                }
                messages
            }
            DriveUpdate::Remove(device_ids) => {
                let mut messages: Vec<SvcMessage> = ledger(&self.ledger)
                    .removed(&device_ids, &mut records)
                    .into_iter()
                    .map(SvcMessage::from)
                    .collect();
                for device_id in device_ids {
                    match rdpdr.remove_drive(device_id) {
                        Ok(removal) => messages.extend(removal),
                        Err(error) => {
                            warn!(device_id, %error, "Failed to remove a redirected drive");
                        }
                    }
                }
                messages
            }
            DriveUpdate::Completion(mut pdu) => {
                if !ledger(&self.ledger).completed(&pdu, &mut records) {
                    warn!("Dropping a drive completion nothing is waiting on");
                    return Ok(records);
                }
                self.setup.policy.completion(&mut pdu);
                vec![SvcMessage::from(pdu)]
            }
        };
        if messages.is_empty() {
            return Ok(records);
        }
        let frame = active_stage
            .process_svc_processor_messages(SvcProcessorMessages::<Rdpdr>::new(messages))
            .context("framing drive messages")?;
        framed
            .write_all(&frame)
            .await
            .context("sending drive messages")?;
        Ok(records)
    }
}
