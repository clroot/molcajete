//! The redirected drive's RDPDR backend: IronRDP's native filesystem
//! backend for this platform, which serves a directory on this host
//! without leaving it, taught what xrdp needs besides Windows.
//!
//! - On Linux and macOS, `NixRdpdrBackend` refuses symbolic links. It
//!   answers directory queries only in the class Windows uses
//!   (`FileBothDirectoryInformation`). xrdp asks for
//!   `FileDirectoryInformation`, and other servers may ask for the full or
//!   names-only classes ([MS-RDPEFS] 2.2.3.3.10 allows all four), so those
//!   queries are answered by the native backend in the class it knows and
//!   the entry rewritten into the one asked for.
//! - On Windows, `WindowsRdpdrBackend` opens every path relative to the
//!   root's handle without following reparse points (symbolic links,
//!   junctions), and answers all four classes itself. It completes waiting
//!   byte-range locks and directory change notifications on worker
//!   threads; the session collects them with
//!   [`RdpdrBackend::poll_deferred_messages`] (see
//!   [`DriveBackend::DEFERS_COMPLETIONS`]).
//! - xrdp separates path components with `/` when it opens files, where
//!   Windows uses `\`; both native backends only take `\`. A `/` cannot be
//!   part of a Windows file name, so it is turned into `\`.

use std::path::Path;

use ironrdp_core::{ReadCursor, impl_as_any};
use ironrdp_pdu::PduResult;
use ironrdp_rdpdr::pdu::RdpdrPdu;
use ironrdp_rdpdr::pdu::efs::{
    AnyIoCtlCode, ClientDriveQueryDirectoryResponse, DecodedDeviceControlRequest,
    DeviceControlRequest, DeviceIoRequest, DeviceIoResponse, FileDirectoryInformation,
    FileFullDirectoryInformation, FileInformationClass, FileInformationClassLevel,
    FileNamesInformation, MajorFunction, MinorFunction, PrinterIoRequest,
    ServerDeviceAnnounceResponse, ServerDriveIoRequest,
};
use ironrdp_rdpdr::pdu::esc::{ScardCall, ScardIoCtlCode};
use ironrdp_rdpdr::pdu::esp::PortIoRequest;
use ironrdp_rdpdr::{Rdpdr, RdpdrBackend};
#[cfg(windows)]
use ironrdp_rdpdr_native::WindowsRdpdrBackend as NativeBackend;
#[cfg(not(windows))]
use ironrdp_rdpdr_native::backend::NixRdpdrBackend as NativeBackend;
use ironrdp_svc::SvcMessage;

/// Whether the native backend answers directory queries only in
/// `FileBothDirectoryInformation`.
const ONLY_BOTH_DIRECTORY_CLASS: bool = !cfg!(windows);

#[derive(Debug)]
pub struct DriveBackend {
    native: NativeBackend,
}

impl_as_any!(DriveBackend);

impl DriveBackend {
    /// Whether the backend completes some requests later, from other
    /// threads, so that its deferred messages must be polled.
    pub const DEFERS_COMPLETIONS: bool = cfg!(windows);

    /// Serves the directory `root` as the drive `device_id`, announced as
    /// `name`. `root` must be an existing directory, resolved; on Windows,
    /// on a drive letter, without the `\\?\` prefix.
    pub fn new(device_id: u32, name: &str, root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            native: native_backend(device_id, name, root)?,
        })
    }

    /// Answers a directory query in another class than the native
    /// backend's.
    fn query_directory_as(
        &mut self,
        mut request: ironrdp_rdpdr::pdu::efs::ServerDriveQueryDirectoryRequest,
        wanted: FileInformationClassLevel,
    ) -> PduResult<Vec<SvcMessage>> {
        request.file_info_class_lvl = FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION;
        let messages = self.native.handle_drive_io_request(
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(request),
        )?;
        Ok(messages
            .into_iter()
            .map(|message| convert(&message, &wanted).unwrap_or(message))
            .collect())
    }
}

#[cfg(not(windows))]
fn native_backend(_device_id: u32, _name: &str, root: &Path) -> std::io::Result<NativeBackend> {
    Ok(NativeBackend::new(root.to_string_lossy().into_owned()))
}

#[cfg(windows)]
fn native_backend(device_id: u32, name: &str, root: &Path) -> std::io::Result<NativeBackend> {
    use ironrdp_rdpdr_native::{RedirectedDrive, WindowsRdpdrBackendFactory};
    let drive = RedirectedDrive::new(device_id, name, root, false)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    Ok(WindowsRdpdrBackendFactory::new(drive).build())
}

/// The directory query response `message` with its entry in the class
/// `wanted`; `None` if it is not one the native backend writes.
fn convert(message: &SvcMessage, wanted: &FileInformationClassLevel) -> Option<SvcMessage> {
    let bytes = message.encode_unframed_pdu().ok()?;
    // The RDPDR header (component, packet ID), then the completion.
    let mut src = ReadCursor::new(bytes.get(4..)?);
    let device_io_response = DeviceIoResponse::decode(&mut src).ok()?;
    let decoded = RdpdrPdu::decode_io_completion(
        MajorFunction::DirectoryControl,
        MinorFunction::IRP_MN_QUERY_DIRECTORY,
        Some(FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION),
        None,
        device_io_response,
        &mut src,
    )
    .ok()?;
    let RdpdrPdu::ClientDriveQueryDirectoryResponse(response) = decoded else {
        return None;
    };
    let buffer = match response.buffer {
        None => None,
        Some(FileInformationClass::BothDirectory(both)) => Some(match *wanted {
            FileInformationClassLevel::FILE_DIRECTORY_INFORMATION => {
                FileInformationClass::Directory(FileDirectoryInformation {
                    allocation_size: both.allocation_size,
                    ..FileDirectoryInformation::new(
                        both.creation_time,
                        both.last_access_time,
                        both.last_write_time,
                        both.change_time,
                        both.end_of_file,
                        both.file_attributes,
                        both.file_name,
                    )
                })
            }
            FileInformationClassLevel::FILE_FULL_DIRECTORY_INFORMATION => {
                FileInformationClass::FullDirectory(FileFullDirectoryInformation {
                    allocation_size: both.allocation_size,
                    ..FileFullDirectoryInformation::new(
                        both.creation_time,
                        both.last_access_time,
                        both.last_write_time,
                        both.change_time,
                        both.end_of_file,
                        both.file_attributes,
                        both.file_name,
                    )
                })
            }
            FileInformationClassLevel::FILE_NAMES_INFORMATION => {
                FileInformationClass::Names(FileNamesInformation::new(both.file_name))
            }
            _ => return None,
        }),
        Some(_) => return None,
    };
    Some(SvcMessage::from(
        RdpdrPdu::ClientDriveQueryDirectoryResponse(ClientDriveQueryDirectoryResponse {
            device_io_reply: response.device_io_reply,
            buffer,
        }),
    ))
}

/// `req` with the paths it names separated by `\`.
fn windows_separators(req: ServerDriveIoRequest) -> ServerDriveIoRequest {
    let fix = |path: &mut String| {
        if path.contains('/') {
            *path = path.replace('/', "\\");
        }
    };
    match req {
        ServerDriveIoRequest::ServerCreateDriveRequest(mut create) => {
            fix(&mut create.path);
            ServerDriveIoRequest::ServerCreateDriveRequest(create)
        }
        ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(mut query) => {
            fix(&mut query.path);
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(query)
        }
        ServerDriveIoRequest::ServerDriveSetInformationRequest(mut set) => {
            if let FileInformationClass::Rename(rename) = &mut set.set_buffer {
                fix(&mut rename.file_name);
            }
            ServerDriveIoRequest::ServerDriveSetInformationRequest(set)
        }
        other => other,
    }
}

impl RdpdrBackend for DriveBackend {
    fn supports_drive_security(&self) -> bool {
        self.native.supports_drive_security()
    }

    fn reset(&mut self) -> PduResult<()> {
        self.native.reset()
    }

    fn restore_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.native.restore_drive(device_id)
    }

    fn add_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.native.add_drive(device_id)
    }

    fn remove_drive(&mut self, device_id: u32) -> PduResult<Vec<SvcMessage>> {
        self.native.remove_drive(device_id)
    }

    fn handle_server_device_announce_response(
        &mut self,
        pdu: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        self.native.handle_server_device_announce_response(pdu)
    }

    fn handle_scard_call(
        &mut self,
        req: DeviceControlRequest<ScardIoCtlCode>,
        call: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        self.native.handle_scard_call(req, call)
    }

    fn handle_drive_io_request(&mut self, req: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        match windows_separators(req) {
            ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(request)
                if ONLY_BOTH_DIRECTORY_CLASS
                    && matches!(
                        request.file_info_class_lvl,
                        FileInformationClassLevel::FILE_DIRECTORY_INFORMATION
                            | FileInformationClassLevel::FILE_FULL_DIRECTORY_INFORMATION
                            | FileInformationClassLevel::FILE_NAMES_INFORMATION
                    ) =>
            {
                let wanted = request.file_info_class_lvl.clone();
                self.query_directory_as(request, wanted)
            }
            other => self.native.handle_drive_io_request(other),
        }
    }

    fn handle_drive_device_control(
        &mut self,
        req: DecodedDeviceControlRequest<AnyIoCtlCode>,
    ) -> PduResult<Vec<SvcMessage>> {
        self.native.handle_drive_device_control(req)
    }

    fn poll_deferred_messages(&mut self) -> PduResult<Vec<SvcMessage>> {
        self.native.poll_deferred_messages()
    }

    fn handle_user_logged_on(&mut self, rdpdr: &mut Rdpdr) -> PduResult<Vec<SvcMessage>> {
        self.native.handle_user_logged_on(rdpdr)
    }

    fn handle_printer_io_request(&mut self, req: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.native.handle_printer_io_request(req)
    }

    fn handle_port_io_request(&mut self, req: PortIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.native.handle_port_io_request(req)
    }

    fn reject_printer_write(&mut self, req: DeviceIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.native.reject_printer_write(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_rdpdr::pdu::efs::{FileAttributes, FileBothDirectoryInformation, NtStatus};

    fn both_response() -> SvcMessage {
        SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(
            ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse {
                    device_id: 1,
                    completion_id: 7,
                    io_status: NtStatus::SUCCESS,
                },
                buffer: Some(FileInformationClass::BothDirectory(
                    FileBothDirectoryInformation::new(
                        1,
                        2,
                        3,
                        4,
                        5,
                        FileAttributes::FILE_ATTRIBUTE_ARCHIVE,
                        "a.txt".into(),
                    ),
                )),
            },
        ))
    }

    fn decode(
        message: &SvcMessage,
        class: FileInformationClassLevel,
    ) -> ClientDriveQueryDirectoryResponse {
        let bytes = message.encode_unframed_pdu().unwrap();
        let mut src = ReadCursor::new(&bytes[4..]);
        let io = DeviceIoResponse::decode(&mut src).unwrap();
        ClientDriveQueryDirectoryResponse::decode_for_class(class, io, &mut src).unwrap()
    }

    #[test]
    fn entries_are_rewritten_into_the_class_asked_for() {
        let message = both_response();
        let directory = convert(
            &message,
            &FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
        )
        .unwrap();
        let decoded = decode(
            &directory,
            FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
        );
        assert_eq!(decoded.device_io_reply.completion_id, 7);
        let Some(FileInformationClass::Directory(entry)) = decoded.buffer else {
            panic!("{:?}", decoded.buffer);
        };
        assert_eq!(entry.file_name, "a.txt");
        assert_eq!(
            (entry.creation_time, entry.change_time, entry.end_of_file),
            (1, 4, 5)
        );

        let names = convert(&message, &FileInformationClassLevel::FILE_NAMES_INFORMATION).unwrap();
        let decoded = decode(&names, FileInformationClassLevel::FILE_NAMES_INFORMATION);
        assert!(
            matches!(decoded.buffer, Some(FileInformationClass::Names(n)) if n.file_name == "a.txt")
        );
    }

    #[test]
    fn xrdp_paths_get_windows_separators() {
        let request = ironrdp_rdpdr::pdu::efs::DeviceCreateRequest {
            device_io_request: DeviceIoRequest {
                device_id: 1,
                file_id: 0,
                completion_id: 1,
                major_function: MajorFunction::Create,
                minor_function: MinorFunction::from(0),
            },
            desired_access: ironrdp_rdpdr::pdu::efs::DesiredAccess::empty(),
            allocation_size: 0,
            file_attributes: FileAttributes::empty(),
            shared_access: ironrdp_rdpdr::pdu::efs::SharedAccess::empty(),
            create_disposition: ironrdp_rdpdr::pdu::efs::CreateDisposition::FILE_OPEN,
            create_options: ironrdp_rdpdr::pdu::efs::CreateOptions::empty(),
            path: "/folder/a.txt".into(),
        };
        let ServerDriveIoRequest::ServerCreateDriveRequest(fixed) =
            windows_separators(ServerDriveIoRequest::ServerCreateDriveRequest(request))
        else {
            unreachable!()
        };
        assert_eq!(fixed.path, "\\folder\\a.txt");
    }

    #[test]
    fn the_end_of_a_listing_stays_as_it_is() {
        let end = SvcMessage::from(RdpdrPdu::ClientDriveQueryDirectoryResponse(
            ClientDriveQueryDirectoryResponse {
                device_io_reply: DeviceIoResponse {
                    device_id: 1,
                    completion_id: 8,
                    io_status: NtStatus::NO_MORE_FILES,
                },
                buffer: None,
            },
        ));
        let converted =
            convert(&end, &FileInformationClassLevel::FILE_DIRECTORY_INFORMATION).unwrap();
        let decoded = decode(
            &converted,
            FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
        );
        assert_eq!(decoded.device_io_reply.io_status, NtStatus::NO_MORE_FILES);
        assert!(decoded.buffer.is_none());
    }
}
