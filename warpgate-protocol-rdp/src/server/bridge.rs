//! Target-side bridging: once auth succeeds, dial the target, pump its framebuffer to the
//! viewer-facing RDP server, and record the session.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc::Sender;
use tracing::{Instrument as _, info, warn};
use warpgate_common::TargetRdpOptions;
use warpgate_core::recordings::DesktopRecorder;
use warpgate_core::{AdmittedTarget, DesktopClientHandles, DesktopEvent, DesktopState, Services};

use super::BackendBridge;
use super::protocol::Input as ServerInput;

/// Bridge target-side desktop events to the viewer-facing RDP server. Every framebuffer
/// update is a delta rectangle, so none may be dropped — a lost tile leaves that region
/// stale until it next changes. `server_in` is bounded, so a slow viewer applies
/// backpressure here, which propagates to the target via IronRDP's frame acknowledgements
/// (the target paces to the slowest consumer instead of us discarding pixels).
///
/// Recording runs on its own task so its (serialising) `write_event` never gates live frame
/// delivery — `DesktopEvent` clones are cheap (`RawImage` holds ref-counted `Bytes`). The
/// recorder is also shared with `control_loop` (viewer input); the recording finalises once
/// every handle drops.
async fn frame_bridge(
    mut event_rx: tokio::sync::mpsc::Receiver<DesktopEvent>,
    server_in: Sender<ServerInput>,
    recorder: Option<Arc<DesktopRecorder>>,
) {
    let record_tx = recorder.map(|recorder| {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<DesktopEvent>(256);
        tokio::spawn(
            async move {
                while let Some(event) = rx.recv().await {
                    if let Err(error) = recorder.write_event(&event).await {
                        warn!(%error, "Failed to record RDP desktop event");
                    }
                }
            }
            .in_current_span(),
        );
        tx
    });

    let mut end = None;
    while let Some(event) = event_rx.recv().await {
        if let Some(record_tx) = &record_tx {
            // Best-effort: recording must never stall the live path, so drop under overload.
            let _ = record_tx.try_send(event.clone());
        }

        let message = match event {
            DesktopEvent::Resize { width, height } => ServerInput::Resize { width, height },
            DesktopEvent::RawImage { rect, data } => ServerInput::Frame {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                // `data` is ref-counted `Bytes` — moved into the frame, no framebuffer copy.
                data,
            },
            DesktopEvent::Cursor { rect, data } => ServerInput::Cursor {
                // As in RFB, the rect's origin is the hotspot.
                hotspot_x: rect.x,
                hotspot_y: rect.y,
                width: rect.width,
                height: rect.height,
                data,
            },
            DesktopEvent::CursorDefault => ServerInput::CursorDefault,
            DesktopEvent::CursorHidden => ServerInput::CursorHidden,
            DesktopEvent::Clipboard(text) => ServerInput::Clipboard(text),
            DesktopEvent::Audio(data) => ServerInput::Audio(data),
            DesktopEvent::MicrophoneOpen {
                format,
                frames_per_packet,
            } => ServerInput::MicrophoneOpen {
                format,
                frames_per_packet,
            },
            DesktopEvent::MicrophoneClose => ServerInput::MicrophoneClose,
            DesktopEvent::State(DesktopState::Disconnected) => {
                let shutdown = match end {
                    Some(code) => ServerInput::Disconnect(code),
                    None => ServerInput::Shutdown,
                };
                let _ = server_in.send(shutdown).await;
                return;
            }
            DesktopEvent::Ended(reason) => {
                info!(message = %reason.message, code = ?reason.rdp_error_info, "RDP target ended the session");
                end = reason.rdp_error_info;
                continue;
            }
            DesktopEvent::Error(message) => {
                warn!(%message, "RDP target reported an error");
                continue;
            }
            DesktopEvent::State(state @ (DesktopState::Connecting | DesktopState::Connected)) => {
                info!(?state, "RDP target");
                continue;
            }
            // The client never emits the remaining framebuffer variants
            _ => continue,
        };

        if server_in.send(message).await.is_err() {
            return;
        }
    }
}

/// Connect to the target and start bridging its framebuffer, once auth is complete.
pub(super) async fn connect_backend(
    services: &Services,
    server_in_tx: &Sender<ServerInput>,
    admitted: AdmittedTarget<TargetRdpOptions>,
    screen: warpgate_desktop_ui::Screen,
) -> Result<BackendBridge> {
    info!(target=%admitted.target().name, "Authorized");

    let recorder = warpgate_desktop_auth::start_recording(services, &admitted.id(), "rdp")
        .await
        .map(Arc::new);

    let DesktopClientHandles {
        event_rx,
        input_tx,
        abort_tx,
        logon_state,
    } = crate::connect(
        admitted,
        (screen.width, screen.height),
        services.secret_backends.clone(),
    )?;
    if let Some(recorder) = &recorder {
        recorder.track_logon_state(logon_state);
    }
    let frame_bridge = tokio::spawn(
        frame_bridge(event_rx, server_in_tx.clone(), recorder.clone()).in_current_span(),
    );
    Ok(BackendBridge {
        input_tx,
        abort_tx,
        frame_bridge,
        recorder,
        dialed_at: Some(screen),
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::sync::mpsc::{self, channel};
    use warpgate_core::{DesktopEndReason, DesktopEvent, DesktopState};

    use super::{ServerInput, frame_bridge};

    #[tokio::test]
    async fn sound_reaches_the_viewer_in_order_with_the_frames() {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (server_tx, mut server_rx) = mpsc::channel(8);
        event_tx
            .send(DesktopEvent::Resize {
                width: 1,
                height: 1,
            })
            .await
            .unwrap();
        event_tx
            .send(DesktopEvent::Audio(Bytes::from_static(&[1, 2, 3, 4])))
            .await
            .unwrap();
        event_tx
            .send(DesktopEvent::State(DesktopState::Disconnected))
            .await
            .unwrap();
        frame_bridge(event_rx, server_tx, None).await;

        assert!(matches!(
            server_rx.recv().await,
            Some(ServerInput::Resize { .. })
        ));
        assert!(
            matches!(server_rx.recv().await, Some(ServerInput::Audio(data)) if data[..] == [1, 2, 3, 4])
        );
        assert!(matches!(
            server_rx.recv().await,
            Some(ServerInput::Shutdown)
        ));
    }

    async fn shutdown_after(events: Vec<DesktopEvent>) -> ServerInput {
        let (event_tx, event_rx) = channel(8);
        let (server_in, mut server_rx) = channel(8);
        for event in events {
            event_tx.send(event).await.unwrap();
        }
        event_tx
            .send(DesktopEvent::State(DesktopState::Disconnected))
            .await
            .unwrap();
        frame_bridge(event_rx, server_in, None).await;
        server_rx.recv().await.unwrap()
    }

    fn ended(rdp_error_info: Option<u32>) -> DesktopEvent {
        DesktopEvent::Ended(DesktopEndReason {
            message: "ended".into(),
            rdp_error_info,
        })
    }

    #[tokio::test]
    async fn the_target_reason_ends_the_viewer_session() {
        assert!(matches!(
            shutdown_after(vec![ended(Some(5))]).await,
            ServerInput::Disconnect(5)
        ));
    }

    #[tokio::test]
    async fn a_target_that_gives_no_code_still_shuts_the_viewer_session() {
        assert!(matches!(
            shutdown_after(vec![ended(None)]).await,
            ServerInput::Shutdown
        ));
        assert!(matches!(
            shutdown_after(vec![]).await,
            ServerInput::Shutdown
        ));
    }

    #[tokio::test]
    async fn the_targets_recording_reaches_the_viewer() {
        let format = warpgate_core::DESKTOP_MICROPHONE_FORMATS[0];
        let (event_tx, event_rx) = mpsc::channel(8);
        let (server_tx, mut server_rx) = mpsc::channel(8);
        event_tx
            .send(DesktopEvent::MicrophoneOpen {
                format,
                frames_per_packet: 441,
            })
            .await
            .unwrap();
        event_tx.send(DesktopEvent::MicrophoneClose).await.unwrap();
        event_tx
            .send(DesktopEvent::State(DesktopState::Disconnected))
            .await
            .unwrap();
        frame_bridge(event_rx, server_tx, None).await;

        assert!(matches!(
            server_rx.recv().await,
            Some(ServerInput::MicrophoneOpen { format: f, frames_per_packet: 441 }) if f == format
        ));
        assert!(matches!(
            server_rx.recv().await,
            Some(ServerInput::MicrophoneClose)
        ));
        assert!(matches!(
            server_rx.recv().await,
            Some(ServerInput::Shutdown)
        ));
    }
}
