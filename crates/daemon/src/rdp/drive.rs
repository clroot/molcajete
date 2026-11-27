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
//!   [`DriveBackend::DEFERS_COMPLETIONS`]). A notification is started on
//!   the calling thread, and Windows cancels I/O whose thread exits, so
//!   notifications are started on a thread the backend keeps for that
//!   (see [`Notifier`]) rather than on the session's, which tokio may end.
//! - xrdp separates path components with `/` when it opens files, where
//!   Windows uses `\`; both native backends only take `\`. A `/` cannot be
//!   part of a Windows file name, so it is turned into `\`.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;

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
    /// Shared with the notifier's thread, which uses it while the caller
    /// waits.
    native: Arc<Mutex<NativeBackend>>,
    /// Where directory change notifications are started, when the native
    /// backend keeps them open.
    notifier: Option<Notifier>,
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
            native: Arc::new(Mutex::new(native_backend(device_id, name, root)?)),
            notifier: if Self::DEFERS_COMPLETIONS {
                Some(Notifier::spawn()?)
            } else {
                None
            },
        })
    }

    fn native(&self) -> MutexGuard<'_, NativeBackend> {
        self.native.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a directory change notification on the notifier's thread,
    /// which outlives it: Windows would cancel it when the thread that
    /// started it exits, and the session's thread can be one of tokio's
    /// blocking threads, which exit after ten idle seconds.
    fn notify_change(&mut self, request: ServerDriveIoRequest) -> PduResult<Vec<SvcMessage>> {
        if let Some(notifier) = &self.notifier {
            let native = Arc::clone(&self.native);
            let request = request.clone();
            if let Some(result) = notifier.run(move || {
                native
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .handle_drive_io_request(request)
            }) {
                return result;
            }
        }
        self.native().handle_drive_io_request(request)
    }

    /// Answers a directory query in another class than the native
    /// backend's.
    fn query_directory_as(
        &mut self,
        mut request: ironrdp_rdpdr::pdu::efs::ServerDriveQueryDirectoryRequest,
        wanted: FileInformationClassLevel,
    ) -> PduResult<Vec<SvcMessage>> {
        request.file_info_class_lvl = FileInformationClassLevel::FILE_BOTH_DIRECTORY_INFORMATION;
        let messages = self.native().handle_drive_io_request(
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
        self.native().supports_drive_security()
    }

    fn reset(&mut self) -> PduResult<()> {
        self.native().reset()
    }

    fn restore_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.native().restore_drive(device_id)
    }

    fn add_drive(&mut self, device_id: u32) -> PduResult<()> {
        self.native().add_drive(device_id)
    }

    fn remove_drive(&mut self, device_id: u32) -> PduResult<Vec<SvcMessage>> {
        self.native().remove_drive(device_id)
    }

    fn handle_server_device_announce_response(
        &mut self,
        pdu: ServerDeviceAnnounceResponse,
    ) -> PduResult<()> {
        self.native().handle_server_device_announce_response(pdu)
    }

    fn handle_scard_call(
        &mut self,
        req: DeviceControlRequest<ScardIoCtlCode>,
        call: ScardCall,
    ) -> PduResult<Vec<SvcMessage>> {
        self.native().handle_scard_call(req, call)
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
            request @ ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(_) => {
                self.notify_change(request)
            }
            other => self.native().handle_drive_io_request(other),
        }
    }

    fn handle_drive_device_control(
        &mut self,
        req: DecodedDeviceControlRequest<AnyIoCtlCode>,
    ) -> PduResult<Vec<SvcMessage>> {
        self.native().handle_drive_device_control(req)
    }

    fn poll_deferred_messages(&mut self) -> PduResult<Vec<SvcMessage>> {
        self.native().poll_deferred_messages()
    }

    fn handle_user_logged_on(&mut self, rdpdr: &mut Rdpdr) -> PduResult<Vec<SvcMessage>> {
        self.native().handle_user_logged_on(rdpdr)
    }

    fn handle_printer_io_request(&mut self, req: PrinterIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.native().handle_printer_io_request(req)
    }

    fn handle_port_io_request(&mut self, req: PortIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.native().handle_port_io_request(req)
    }

    fn reject_printer_write(&mut self, req: DeviceIoRequest) -> PduResult<Vec<SvcMessage>> {
        self.native().reject_printer_write(req)
    }
}

/// A thread that runs jobs one at a time for as long as the backend
/// lives, so that I/O they start is not cancelled with their thread.
#[derive(Debug)]
struct Notifier {
    jobs: Option<mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

type Job = Box<dyn FnOnce() + Send>;

impl Notifier {
    fn spawn() -> std::io::Result<Self> {
        let (jobs, received) = mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("molcajete-rdpdr-notify".into())
            .spawn(move || {
                for job in received {
                    job();
                }
            })?;
        Ok(Self {
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    /// Runs `job` on the thread and waits for its result; `None` if the
    /// thread is gone.
    fn run<R: Send + 'static>(&self, job: impl FnOnce() -> R + Send + 'static) -> Option<R> {
        let (result, received) = mpsc::sync_channel(1);
        self.jobs
            .as_ref()?
            .send(Box::new(move || {
                let _ = result.send(job());
            }))
            .ok()?;
        received.recv().ok()
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
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

    // A server working in the drive, request by request, as `Rdpdr`
    // hands them to the backend: xrdp's separators and directory class,
    // a link out of the folder, and (Windows) a change notification.

    fn header(file_id: u32, major_function: MajorFunction) -> DeviceIoRequest {
        DeviceIoRequest {
            device_id: 1,
            file_id,
            completion_id: 7,
            major_function,
            minor_function: MinorFunction::from(0),
        }
    }

    fn status(messages: &[SvcMessage]) -> NtStatus {
        let bytes = messages[0].encode_unframed_pdu().unwrap();
        NtStatus::from(u32::from_le_bytes(bytes[12..16].try_into().unwrap()))
    }

    /// Opens `path`; the file id, or the failure.
    fn open(
        backend: &mut DriveBackend,
        path: &str,
        disposition: ironrdp_rdpdr::pdu::efs::CreateDisposition,
        directory: bool,
    ) -> Result<u32, NtStatus> {
        use ironrdp_rdpdr::pdu::efs::{CreateOptions, DesiredAccess, SharedAccess};
        let response = backend
            .handle_drive_io_request(ServerDriveIoRequest::ServerCreateDriveRequest(
                ironrdp_rdpdr::pdu::efs::DeviceCreateRequest {
                    device_io_request: header(0, MajorFunction::Create),
                    // Read, write, append, read attributes, synchronize.
                    desired_access: DesiredAccess::from_bits_retain(0x0010_0087),
                    allocation_size: 0,
                    file_attributes: if directory {
                        FileAttributes::FILE_ATTRIBUTE_DIRECTORY
                    } else {
                        FileAttributes::FILE_ATTRIBUTE_NORMAL
                    },
                    shared_access: SharedAccess::from_bits_retain(0x0000_0007),
                    create_disposition: disposition,
                    create_options: if directory {
                        CreateOptions::FILE_DIRECTORY_FILE
                    } else {
                        CreateOptions::FILE_NON_DIRECTORY_FILE
                    },
                    path: path.to_owned(),
                },
            ))
            .unwrap();
        match status(&response) {
            NtStatus::SUCCESS => {
                let bytes = response[0].encode_unframed_pdu().unwrap();
                Ok(u32::from_le_bytes(bytes[16..20].try_into().unwrap()))
            }
            failure => Err(failure),
        }
    }

    fn close(backend: &mut DriveBackend, file_id: u32) {
        let response = backend
            .handle_drive_io_request(ServerDriveIoRequest::DeviceCloseRequest(
                ironrdp_rdpdr::pdu::efs::DeviceCloseRequest {
                    device_io_request: header(file_id, MajorFunction::Close),
                },
            ))
            .unwrap();
        assert_eq!(status(&response), NtStatus::SUCCESS);
    }

    /// The names in directory `file_id`, listed as xrdp asks.
    fn list(backend: &mut DriveBackend, file_id: u32, pattern: &str) -> Vec<String> {
        let mut names = Vec::new();
        for initial_query in std::iter::once(1).chain(std::iter::repeat(0)) {
            let mut request = header(file_id, MajorFunction::DirectoryControl);
            request.minor_function = MinorFunction::IRP_MN_QUERY_DIRECTORY;
            let response = backend
                .handle_drive_io_request(ServerDriveIoRequest::ServerDriveQueryDirectoryRequest(
                    ironrdp_rdpdr::pdu::efs::ServerDriveQueryDirectoryRequest {
                        device_io_request: request,
                        file_info_class_lvl: FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
                        initial_query,
                        path: pattern.to_owned(),
                    },
                ))
                .unwrap();
            let decoded = decode(
                &response[0],
                FileInformationClassLevel::FILE_DIRECTORY_INFORMATION,
            );
            match decoded.buffer {
                Some(FileInformationClass::Directory(entry)) => names.push(entry.file_name),
                None => {
                    assert_eq!(decoded.device_io_reply.io_status, NtStatus::NO_MORE_FILES);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        names.retain(|name| name != "." && name != "..");
        names.sort();
        names
    }

    /// A link at `link` to the directory `target`: a symbolic link, or on
    /// Windows a junction, which needs no privileges.
    fn link_directory(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        assert!(
            std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn a_server_works_in_the_drive_and_stays_inside_it() {
        use ironrdp_rdpdr::pdu::efs::{CreateDisposition, DeviceWriteRequest};

        let base = std::env::temp_dir().join(format!("molcajete-rdpdr-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&base).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        // Opened as for a session.
        let drive = crate::files::Drive::open(crate::files::DriveSettings {
            path: base.join("drive"),
            name: "Drive".into(),
            create: true,
            policy: crate::files::Policy {
                download: true,
                upload: true,
                max_size: None,
            },
        })
        .await
        .unwrap();
        let root = drive.root().to_owned();
        let mut backend = DriveBackend::new(1, "Drive", &root).unwrap();
        backend.restore_drive(1).unwrap();

        // A folder and a file in it, with xrdp's `/` separators.
        let folder = open(
            &mut backend,
            "/folder",
            CreateDisposition::FILE_CREATE,
            true,
        )
        .unwrap();
        close(&mut backend, folder);
        let file = open(
            &mut backend,
            "/folder/a.txt",
            CreateDisposition::FILE_OVERWRITE_IF,
            false,
        )
        .unwrap();
        let written = backend
            .handle_drive_io_request(ServerDriveIoRequest::DeviceWriteRequest(
                DeviceWriteRequest {
                    device_io_request: header(file, MajorFunction::Write),
                    offset: 0,
                    write_data: b"hello".to_vec(),
                },
            ))
            .unwrap();
        assert_eq!(status(&written), NtStatus::SUCCESS);
        close(&mut backend, file);
        assert_eq!(
            std::fs::read(root.join("folder").join("a.txt")).unwrap(),
            b"hello"
        );

        // Listed in the class xrdp asks for.
        let folder = open(&mut backend, "/folder", CreateDisposition::FILE_OPEN, true).unwrap();
        assert_eq!(list(&mut backend, folder, "/folder/*"), ["a.txt"]);
        close(&mut backend, folder);

        // A link placed in the folder does not lead out of it.
        link_directory(&outside, &root.join("link"));
        assert!(
            open(
                &mut backend,
                "/link/secret.txt",
                CreateDisposition::FILE_OPEN,
                false
            )
            .is_err()
        );
        assert!(
            open(
                &mut backend,
                "/link/new.txt",
                CreateDisposition::FILE_OVERWRITE_IF,
                false
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);

        // A file uploaded from the browser is announced to a server that
        // watches the folder, through the deferred completions the
        // session polls.
        if DriveBackend::DEFERS_COMPLETIONS {
            let watched =
                open(&mut backend, "/folder", CreateDisposition::FILE_OPEN, true).unwrap();
            assert!(watch(&mut backend, watched).is_empty());
            assert!(backend.poll_deferred_messages().unwrap().is_empty());
            std::fs::write(root.join("folder").join("uploaded.txt"), b"x").unwrap();
            let completed = completion(&mut backend);
            assert_eq!(status(&completed), NtStatus::SUCCESS);
            close(&mut backend, watched);
        }

        backend.reset().unwrap();
        drop(backend);
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Asks for a notification when a file name changes in the directory
    /// `file_id`; what the backend answers at once.
    fn watch(backend: &mut DriveBackend, file_id: u32) -> Vec<SvcMessage> {
        let mut request = header(file_id, MajorFunction::DirectoryControl);
        request.minor_function = MinorFunction::IRP_MN_NOTIFY_CHANGE_DIRECTORY;
        backend
            .handle_drive_io_request(
                ServerDriveIoRequest::ServerDriveNotifyChangeDirectoryRequest(
                    ironrdp_rdpdr::pdu::efs::ServerDriveNotifyChangeDirectoryRequest {
                        device_io_request: request,
                        watch_tree: 0,
                        // FILE_NOTIFY_CHANGE_FILE_NAME
                        completion_filter: 1,
                    },
                ),
            )
            .unwrap()
    }

    /// The deferred completions, once there are some.
    fn completion(backend: &mut DriveBackend) -> Vec<SvcMessage> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let messages = backend.poll_deferred_messages().unwrap();
            if !messages.is_empty() {
                return messages;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no change notification"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A notification asked for on a thread that then exits, as tokio's
    /// blocking threads do, stays open until the folder changes.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_notification_outlives_the_thread_that_asked_for_it() {
        use ironrdp_rdpdr::pdu::efs::CreateDisposition;

        let base = std::env::temp_dir().join(format!("molcajete-rdpdr-{}", uuid::Uuid::new_v4()));
        let drive = crate::files::Drive::open(crate::files::DriveSettings {
            path: base.clone(),
            name: "Drive".into(),
            create: true,
            policy: crate::files::Policy {
                download: true,
                upload: true,
                max_size: None,
            },
        })
        .await
        .unwrap();
        let root = drive.root().to_owned();
        let mut backend = DriveBackend::new(1, "Drive", &root).unwrap();
        backend.restore_drive(1).unwrap();
        let watched = open(&mut backend, "\\", CreateDisposition::FILE_OPEN, true).unwrap();

        let mut backend = std::thread::spawn(move || {
            assert!(watch(&mut backend, watched).is_empty());
            backend
        })
        .join()
        .unwrap();
        // Long enough for Windows to cancel the I/O of the exited thread.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(backend.poll_deferred_messages().unwrap().is_empty());

        std::fs::write(root.join("new.txt"), b"x").unwrap();
        assert_eq!(status(&completion(&mut backend)), NtStatus::SUCCESS);
        close(&mut backend, watched);

        backend.reset().unwrap();
        drop(backend);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
