//! Text clipboard over the CLIPRDR static virtual channel.
//!
//! IronRDP's [`Cliprdr`](ironrdp_cliprdr::Cliprdr) calls the backend
//! synchronously while it processes channel data. The backend therefore only
//! records what has to happen ([`Action`]s); the session loop drains them
//! after each call into the active stage, turns them into channel messages
//! and forwards remote text to the Guacamole client.

use ironrdp_cliprdr::Client;
use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_cliprdr::{CliprdrClient, CliprdrSvcMessages};
use ironrdp_pdu::PduResult;

/// Something the session loop has to do on behalf of the backend.
#[derive(Debug)]
pub enum Action {
    /// Announce the formats of the local (Guacamole) clipboard.
    Advertise(Vec<ClipboardFormat>),
    /// Ask the server for its clipboard contents in this format.
    Request(ClipboardFormatId),
    /// Answer a data request of the server.
    Respond(OwnedFormatDataResponse),
    /// Text copied on the remote desktop, for the Guacamole client.
    RemoteText(String),
}

/// Clipboard backend bridging CLIPRDR and the Guacamole clipboard.
#[derive(Debug, Default)]
pub struct GuacClipboard {
    /// Latest text received from the Guacamole client.
    local_text: Option<String>,
    /// Format of the outstanding data request, to decode the response.
    requested: Option<ClipboardFormatId>,
    /// Whether remote clipboard contents may be fetched (`disable-copy`).
    fetch_remote: bool,
    /// Whether the channel finished its initialization.
    ready: bool,
    actions: Vec<Action>,
}

ironrdp_core::impl_as_any!(GuacClipboard);

impl GuacClipboard {
    pub fn new(fetch_remote: bool) -> Self {
        Self {
            fetch_remote,
            ..Self::default()
        }
    }

    /// Takes the pending actions.
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    /// Stores new local text and, once the channel is up, announces it to
    /// the server (which then requests the data when it needs it).
    pub fn set_local_text(&mut self, text: String) {
        self.local_text = Some(text);
        if self.ready {
            self.actions.push(Action::Advertise(self.local_formats()));
        }
    }

    fn local_formats(&self) -> Vec<ClipboardFormat> {
        match self.local_text {
            Some(_) => vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)],
            None => Vec::new(),
        }
    }
}

/// Text format to request among those offered by the server.
fn preferred_text_format(formats: &[ClipboardFormat]) -> Option<ClipboardFormatId> {
    [
        ClipboardFormatId::CF_UNICODETEXT,
        ClipboardFormatId::CF_TEXT,
    ]
    .into_iter()
    .find(|wanted| formats.iter().any(|f| f.id() == *wanted))
}

impl CliprdrBackend for GuacClipboard {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        // Text only: long format names are needed by most servers, files
        // and locking are not used.
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        self.ready = true;
    }

    fn on_request_format_list(&mut self) {
        // Completes the channel initialization (even with no formats).
        self.actions.push(Action::Advertise(self.local_formats()));
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        if !self.fetch_remote {
            return;
        }
        if let Some(format) = preferred_text_format(available_formats) {
            self.requested = Some(format);
            self.actions.push(Action::Request(format));
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = match (&self.local_text, request.format) {
            (Some(text), ClipboardFormatId::CF_UNICODETEXT) => {
                OwnedFormatDataResponse::new_unicode_string(text)
            }
            (Some(text), ClipboardFormatId::CF_TEXT) => OwnedFormatDataResponse::new_string(text),
            _ => OwnedFormatDataResponse::new_error(),
        };
        self.actions.push(Action::Respond(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let Some(format) = self.requested.take() else {
            return;
        };
        if response.is_error() {
            return;
        }
        let text = if format == ClipboardFormatId::CF_UNICODETEXT {
            response.to_unicode_string()
        } else {
            response.to_string()
        };
        if let Ok(text) = text {
            self.actions.push(Action::RemoteText(text));
        }
    }

    fn on_file_contents_request(&mut self, _: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}
}

/// Converts backend actions into channel messages, collecting remote text.
pub fn messages_for(
    cliprdr: &mut CliprdrClient,
    actions: Vec<Action>,
    remote_text: &mut Option<String>,
) -> PduResult<Vec<CliprdrSvcMessages<Client>>> {
    let mut messages = Vec::new();
    for action in actions {
        match action {
            Action::Advertise(formats) => messages.push(cliprdr.initiate_copy(&formats)?),
            Action::Request(format) => messages.push(cliprdr.initiate_paste(format)?),
            Action::Respond(response) => messages.push(cliprdr.submit_format_data(response)?),
            Action::RemoteText(text) => *remote_text = Some(text),
        }
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unicode() -> ClipboardFormat {
        ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
    }

    #[test]
    fn initialization_advertises_local_text() {
        let mut cb = GuacClipboard::new(true);
        cb.on_request_format_list();
        assert!(matches!(&cb.take_actions()[..], [Action::Advertise(f)] if f.is_empty()));
        cb.set_local_text("hi".into());
        cb.on_request_format_list();
        assert!(
            matches!(&cb.take_actions()[..], [Action::Advertise(f)] if f[0].id() == ClipboardFormatId::CF_UNICODETEXT)
        );
    }

    #[test]
    fn local_text_is_advertised_once_ready() {
        let mut cb = GuacClipboard::new(true);
        // Before initialization completes, the text is only stored.
        cb.set_local_text("early".into());
        assert!(cb.take_actions().is_empty());
        cb.on_ready();
        cb.set_local_text("later".into());
        assert!(matches!(&cb.take_actions()[..], [Action::Advertise(f)] if f.len() == 1));
    }

    #[test]
    fn remote_copy_fetches_text() {
        let mut cb = GuacClipboard::new(true);
        let formats = [ClipboardFormat::new(ClipboardFormatId::CF_TEXT), unicode()];
        cb.on_remote_copy(&formats);
        assert!(matches!(
            &cb.take_actions()[..],
            [Action::Request(ClipboardFormatId::CF_UNICODETEXT)]
        ));
        cb.on_format_data_response(FormatDataResponse::new_unicode_string("안녕\nworld"));
        match &cb.take_actions()[..] {
            [Action::RemoteText(text)] => assert_eq!(text, "안녕\nworld"),
            other => panic!("unexpected actions: {other:?}"),
        }
        // Images and other formats are ignored.
        cb.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_DIB)]);
        assert!(cb.take_actions().is_empty());
    }

    #[test]
    fn ansi_text_is_decoded_too() {
        let mut cb = GuacClipboard::new(true);
        cb.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_TEXT)]);
        cb.take_actions();
        cb.on_format_data_response(FormatDataResponse::new_string("plain"));
        assert!(matches!(&cb.take_actions()[..], [Action::RemoteText(t)] if t == "plain"));
    }

    #[test]
    fn disable_copy_never_fetches() {
        let mut cb = GuacClipboard::new(false);
        cb.on_remote_copy(&[unicode()]);
        assert!(cb.take_actions().is_empty());
    }

    #[test]
    fn data_requests_are_answered_from_local_text() {
        let mut cb = GuacClipboard::new(true);
        cb.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert!(matches!(&cb.take_actions()[..], [Action::Respond(r)] if r.is_error()));
        cb.set_local_text("héllo".into());
        cb.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        match &cb.take_actions()[..] {
            [Action::Respond(r)] => assert_eq!(r.to_unicode_string().unwrap(), "héllo"),
            other => panic!("unexpected actions: {other:?}"),
        }
    }

    #[test]
    fn unrequested_or_failed_responses_are_dropped() {
        let mut cb = GuacClipboard::new(true);
        cb.on_format_data_response(FormatDataResponse::new_unicode_string("x"));
        assert!(cb.take_actions().is_empty());
        cb.on_remote_copy(&[unicode()]);
        cb.take_actions();
        cb.on_format_data_response(FormatDataResponse::new_error());
        assert!(cb.take_actions().is_empty());
    }
}
