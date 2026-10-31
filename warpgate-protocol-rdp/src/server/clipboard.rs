use std::sync::{Arc, Mutex, PoisonError};

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc::UnboundedSender;
use warpgate_core::{ClipboardTransfer, DesktopInput};

use super::protocol::Event;
use crate::clipboard::{Clipboard, ClipboardSink};

type ServerSender = Arc<Mutex<Option<UnboundedSender<ServerEvent>>>>;

#[derive(Clone, Debug)]
struct ViewerSink {
    server: ServerSender,
    to_warpgate: UnboundedSender<Event>,
}

impl ClipboardSink for ViewerSink {
    fn request(&self, message: ClipboardMessage) {
        if let Some(server) = self
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = server.send(ServerEvent::Clipboard(message));
        }
    }

    fn text_received(&self, text: String) {
        let _ = self
            .to_warpgate
            .send(Event::Input(DesktopInput::Clipboard(text)));
    }

    fn transfer(&self, transfer: ClipboardTransfer) {
        let _ = self
            .to_warpgate
            .send(Event::Input(DesktopInput::ClipboardTransfer(transfer)));
    }
}

#[derive(Clone, Debug)]
pub(super) struct ViewerClipboard(Clipboard<ViewerSink>);

impl ViewerClipboard {
    pub(super) fn new(to_warpgate: UnboundedSender<Event>) -> Self {
        // Files are advertised from the start, since the channel comes up before the target
        // is chosen, but none move until the target allows them (`set_file_limit`). The
        // target-facing channel audits transfers, so this one doesn't.
        Self(
            Clipboard::new(ViewerSink {
                server: ServerSender::default(),
                to_warpgate,
            })
            .with_files(None, false),
        )
    }

    pub(super) fn offer(&self, text: String) {
        self.0.offer(text);
    }

    pub(super) fn transfer(&self, transfer: ClipboardTransfer) {
        self.0.transfer(transfer);
    }

    pub(super) fn set_file_limit(&self, max_bytes: Option<u64>) {
        self.0.set_file_limit(max_bytes);
    }
}

impl CliprdrBackendFactory for ViewerClipboard {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(self.0.backend())
    }
}

impl ServerEventSender for ViewerClipboard {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        *self
            .0
            .sink()
            .server
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(sender);
    }
}

impl CliprdrServerFactory for ViewerClipboard {}
