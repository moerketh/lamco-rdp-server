//! Staging File Transfer Backend
//!
//! Downloads all clipboard files from Windows upfront to a temp directory,
//! then announces the file paths via the clipboard provider. Works in all
//! deployment contexts including Flatpak sandboxes.
//!
//! # Download Flow
//!
//! 1. `prepare_files()` creates temp files and sends FileContentsRequest for each
//! 2. `deliver_file_data()` writes chunks, sends continuation requests for large files
//! 3. When all files complete: emits `FileTransferEvent::FilesReady`
//! 4. Orchestrator announces paths via `text/uri-list` + `gnome-copied-files`

use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use ironrdp_cliprdr::{
    backend::ClipboardMessage,
    pdu::{
        FileContentsFlags, FileContentsRequest as CliprdrFileContentsRequest, FileContentsResponse,
    },
};
use ironrdp_server::ServerEvent;
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::{
    FileTransferBackend, FileTransferEvent, OutgoingFileInfo, PrepareResult, TransferFileDescriptor,
};
use crate::clipboard::error::{ClipboardError, Result};

/// Maximum bytes per FileContentsRequest chunk (64 MB)
const MAX_CHUNK_SIZE: u64 = 64 * 1024 * 1024;

/// Create the directory a staged entry lands in (it may sit inside a copied
/// directory whose own descriptor has not been processed).
fn create_parent_dir(path: &std::path::Path) -> std::io::Result<()> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent),
        None => Ok(()),
    }
}

/// A file being received from Windows (download in progress).
#[derive(Debug)]
struct IncomingFile {
    #[expect(dead_code, reason = "mirrors HashMap key for Debug output")]
    stream_id: u32,
    filename: String,
    total_size: u64,
    received_size: u64,
    temp_path: PathBuf,
    /// Where the finished file goes: its place in the copied tree.
    final_path: PathBuf,
    /// Announce `final_path` once done (a copied item, not a nested entry).
    top_level: bool,
    file_handle: File,
    /// Index in the FileGroupDescriptorW list (for continuation requests)
    file_index: u32,
    /// Clipboard data lock ID. Upstream Cliprdr auto-fills `data_id` from
    /// `current_lock_id` on continuation requests, so we no longer pass this
    /// explicitly; kept for Debug output and future lock-aware diagnostics.
    #[expect(
        dead_code,
        reason = "Cliprdr auto-fills data_id from current_lock_id; field retained for Debug"
    )]
    clip_data_id: u32,
}

/// Internal transfer state for the staging backend.
#[derive(Debug)]
struct StagingState {
    /// Incoming files being downloaded (stream_id → state)
    incoming_files: HashMap<u32, IncomingFile>,
    /// Outgoing files (Linux → Windows)
    outgoing_files: Vec<OutgoingFileInfo>,
    /// Download directory for completed files
    download_dir: PathBuf,
    /// Portal serial for the current incoming transfer
    portal_serial: Option<u32>,
    /// Completed file paths (after rename from temp)
    completed_files: Vec<PathBuf>,
    /// Server event sender for continuation requests
    /// Stored during prepare_files so deliver_file_data can send followups
    server_event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl StagingState {
    fn new(download_dir: PathBuf) -> Self {
        Self {
            incoming_files: HashMap::new(),
            outgoing_files: Vec::new(),
            download_dir,
            portal_serial: None,
            completed_files: Vec::new(),
            server_event_sender: None,
        }
    }

    fn clear_incoming(&mut self) {
        self.incoming_files.clear();
        self.portal_serial = None;
        self.completed_files.clear();
    }
}

/// Staging file transfer backend.
///
/// Downloads all clipboard files from Windows immediately, stores them
/// in the configured download directory, and emits `FileTransferEvent::FilesReady`
/// when all files are complete.
pub struct StagingFileTransfer {
    state: RwLock<StagingState>,
    next_stream_id: AtomicU32,
    event_tx: mpsc::UnboundedSender<FileTransferEvent>,
    event_rx: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<FileTransferEvent>>>,
}

impl StagingFileTransfer {
    pub fn new(download_dir: PathBuf) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            state: RwLock::new(StagingState::new(download_dir)),
            next_stream_id: AtomicU32::new(1),
            event_tx,
            event_rx: parking_lot::Mutex::new(Some(event_rx)),
        }
    }

    /// Read a chunk from a local file for outgoing transfer.
    fn read_file_chunk(path: &PathBuf, offset: u64, size: u32) -> Result<Vec<u8>> {
        let mut file = File::open(path)
            .map_err(|e| ClipboardError::FileIoError(format!("Failed to open file: {e}")))?;

        file.seek(SeekFrom::Start(offset)).map_err(|e| {
            ClipboardError::FileIoError(format!("Failed to seek to offset {offset}: {e}"))
        })?;

        let mut buffer = vec![0u8; size as usize];
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| ClipboardError::FileIoError(format!("Failed to read file: {e}")))?;

        buffer.truncate(bytes_read);
        Ok(buffer)
    }
}

#[async_trait]
impl FileTransferBackend for StagingFileTransfer {
    fn name(&self) -> &'static str {
        "Staging Download"
    }

    fn requires_eager_download(&self) -> bool {
        true
    }

    async fn initialize(&mut self) -> Result<()> {
        let state = self.state.read();
        if let Err(e) = std::fs::create_dir_all(&state.download_dir) {
            warn!(
                "Failed to create download directory '{}': {}",
                state.download_dir.display(),
                e
            );
        }
        info!(
            "Staging file transfer initialized (download_dir={})",
            state.download_dir.display()
        );
        Ok(())
    }

    async fn prepare_files(
        &self,
        descriptors: &[TransferFileDescriptor],
        portal_serial: u32,
        server_event_sender: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<PrepareResult> {
        info!(
            "Staging: preparing {} file(s) for download",
            descriptors.len()
        );

        let mut state = self.state.write();
        state.clear_incoming();
        state.portal_serial = Some(portal_serial);
        state.server_event_sender = Some(server_event_sender.clone());

        // Lock emission removed (D.4.1, 2026-05-16). Upstream IronRDP's #1166 made
        // Lock/Unlock automatic: Cliprdr emits LockData inside handle_format_list()
        // immediately after sending FormatListResponse when the format list contains
        // FileGroupDescriptorW, and Unlock is emitted by expire_all_locks() on the
        // configured lock timeout (see Cliprdr::with_lock_timeouts). The auto-assigned
        // clip_data_id is tracked as Cliprdr::current_lock_id and auto-injected into
        // FileContentsRequest.data_id by request_file_contents() when the caller passes
        // data_id: None. We do that in the SendFileContentsRequest call sites below.
        info!("Cliprdr now manages Lock/Unlock automatically; no explicit emission needed");

        if let Err(e) = std::fs::create_dir_all(&state.download_dir) {
            error!("Failed to create download directory: {}", e);
            return Ok(PrepareResult::Failed(format!(
                "Cannot create download dir: {e}"
            )));
        }

        for desc in descriptors {
            let final_path = state.download_dir.join(&desc.relative_path);

            if desc.is_directory || desc.size == 0 {
                // Nothing to fetch for a directory or a zero-byte file. A RANGE
                // request past the end of the stream is rejected by IronRDP's
                // request_file_contents() as a PDU error that drops the whole
                // connection, so materialise the entry locally instead.
                let made = if desc.is_directory {
                    std::fs::create_dir_all(&final_path)
                } else {
                    create_parent_dir(&final_path).and_then(|()| File::create(&final_path).map(drop))
                };
                match made {
                    Ok(()) => {
                        info!("Staged empty entry: {}", final_path.display());
                        if desc.top_level {
                            state.completed_files.push(final_path);
                        }
                    }
                    Err(e) => error!(
                        "Failed to create '{}': {}",
                        final_path.display(),
                        e
                    ),
                }
                continue;
            }

            let stream_id = self.next_stream_id.fetch_add(1, Ordering::Relaxed);

            info!(
                "Requesting file: '{}' ({} bytes, stream_id={})",
                desc.filename, desc.size, stream_id
            );

            let temp_path = state
                .download_dir
                .join(format!(".{}.{stream_id}.tmp", desc.filename));

            let file_handle = match File::create(&temp_path) {
                Ok(f) => f,
                Err(e) => {
                    error!(
                        "Failed to create temp file '{}': {}",
                        temp_path.display(),
                        e
                    );
                    continue;
                }
            };

            let incoming = IncomingFile {
                stream_id,
                filename: desc.filename.clone(),
                total_size: desc.size,
                received_size: 0,
                temp_path,
                final_path,
                top_level: desc.top_level,
                file_handle,
                file_index: desc.file_index,
                clip_data_id: desc.clip_data_id,
            };
            state.incoming_files.insert(stream_id, incoming);

            let request_size = desc.size.min(MAX_CHUNK_SIZE) as u32;

            match server_event_sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsRequest(CliprdrFileContentsRequest {
                    stream_id,
                    // IronRDP changed index from u32 to i32 to match MS-RDPECLIP
                    // 2.2.5.3 (signed 32-bit lindex). Our internal file_index is
                    // u32; cast at the boundary since values originated from
                    // FileGroupDescriptor parsing which already validates non-negative.
                    index: desc.file_index as i32,
                    // IronRDP renamed FileContentsFlags::DATA to RANGE
                    // (same value 0x0000_0002) for spec alignment in #1166.
                    flags: FileContentsFlags::RANGE,
                    position: 0,
                    requested_size: request_size,
                    // data_id: None tells IronRDP's request_file_contents() to
                    // auto-fill from current_lock_id (the auto-emitted Lock from
                    // handle_format_list when the file format was detected).
                    // Our previous desc.clip_data_id was a downstream guess at
                    // the lock id; upstream-tracked value is authoritative.
                    data_id: None,
                }),
            )) {
                Err(e) => {
                    error!(
                        "Failed to send FileContentsRequest for '{}': {:?}",
                        desc.filename, e
                    );
                }
                _ => {
                    info!(
                        "Sent FileContentsRequest for '{}' (stream={}, {} bytes)",
                        desc.filename, stream_id, request_size
                    );
                }
            }
        }

        if state.incoming_files.is_empty() {
            // Every entry was zero-byte: nothing is in flight, so no
            // FileContentsResponse will ever complete the paste.
            let paths = std::mem::take(&mut state.completed_files);
            state.portal_serial = None;
            return Ok(if paths.is_empty() {
                PrepareResult::Failed("No files could be staged".to_string())
            } else {
                PrepareResult::Ready(paths)
            });
        }

        info!(
            "Initiated staging transfer for {} file(s), waiting for responses...",
            state.incoming_files.len()
        );

        Ok(PrepareResult::Pending)
    }

    async fn deliver_file_data(&self, stream_id: u32, data: Vec<u8>, is_error: bool) -> Result<()> {
        if is_error {
            warn!("FileContentsResponse ERROR: stream={}", stream_id);
            let mut state = self.state.write();
            if let Some(file) = state.incoming_files.remove(&stream_id) {
                info!("Cleaning up failed transfer: {}", file.filename);
                let _ = std::fs::remove_file(&file.temp_path);
            }

            if let Some(serial) = state.portal_serial.take() {
                let _ = self.event_tx.send(FileTransferEvent::TransferFailed {
                    reason: format!("RDP error on stream {stream_id}"),
                    portal_serial: serial,
                });
            }
            return Ok(());
        }

        info!(
            "FileContentsResponse: stream={}, {} bytes",
            stream_id,
            data.len()
        );

        let mut state = self.state.write();

        let file = match state.incoming_files.get_mut(&stream_id) {
            Some(f) => f,
            None => {
                warn!(
                    "Received FileContentsResponse for unknown stream {}",
                    stream_id
                );
                return Ok(());
            }
        };

        file.file_handle.write_all(&data).map_err(|e| {
            error!(
                "Failed to write {} bytes to '{}': {}",
                data.len(),
                file.temp_path.display(),
                e
            );
            ClipboardError::FileIoError(format!("File write failed: {e}"))
        })?;

        file.received_size += data.len() as u64;

        let percent = if file.total_size > 0 {
            (file.received_size as f64 / file.total_size as f64) * 100.0
        } else {
            100.0
        };
        info!(
            "Progress: '{}' - {}/{} bytes ({:.1}%)",
            file.filename,
            file.received_size,
            if file.total_size > 0 {
                file.total_size
            } else {
                file.received_size
            },
            percent
        );

        let file_complete = file.total_size > 0 && file.received_size >= file.total_size;

        if file_complete {
            debug!("File transfer complete: '{}'", file.filename);

            file.file_handle
                .flush()
                .map_err(|e| ClipboardError::FileIoError(format!("Failed to flush file: {e}")))?;

            let temp_path = file.temp_path.clone();
            let final_path = file.final_path.clone();
            if file.top_level {
                state.completed_files.push(final_path.clone());
            }
            state.incoming_files.remove(&stream_id);

            let all_complete = state.incoming_files.is_empty();
            let portal_serial = state.portal_serial;
            let completed_files = state.completed_files.clone();

            // Release lock before file rename
            drop(state);

            create_parent_dir(&final_path)
                .and_then(|()| std::fs::rename(&temp_path, &final_path))
                .map_err(|e| {
                error!(
                    "Failed to move '{}' to '{}': {}",
                    temp_path.display(),
                    final_path.display(),
                    e
                );
                ClipboardError::FileIoError(format!("Failed to finalize file: {e}"))
            })?;

            info!("Saved file to: {}", final_path.display());

            if all_complete {
                debug!(
                    "All {} file(s) transferred successfully",
                    completed_files.len()
                );

                if let Some(serial) = portal_serial {
                    let _ = self.event_tx.send(FileTransferEvent::FilesReady {
                        paths: completed_files,
                        portal_serial: serial,
                    });
                }

                let mut state = self.state.write();
                state.completed_files.clear();
                state.portal_serial = None;
            }
        } else if file.total_size > 0 {
            // Request next chunk
            let remaining = file.total_size - file.received_size;
            let next_chunk_size = remaining.min(MAX_CHUNK_SIZE) as u32;
            let position = file.received_size;
            let file_index = file.file_index;
            let filename = file.filename.clone();
            let sender = state.server_event_sender.clone();

            // Release lock before sending
            drop(state);

            if let Some(ref sender) = sender {
                info!(
                    "Requesting next chunk for '{}' (pos={}, size={}, remaining={})",
                    filename, position, next_chunk_size, remaining
                );

                if let Err(e) = sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendFileContentsRequest(CliprdrFileContentsRequest {
                        stream_id,
                        index: file_index as i32,
                        flags: FileContentsFlags::RANGE,
                        position,
                        requested_size: next_chunk_size,
                        // None → upstream auto-fills from current_lock_id.
                        // Continuation requests within the same paste belong to
                        // the same lock, so the auto-fill is correct.
                        data_id: None,
                    }),
                )) {
                    error!("Failed to send continuation FileContentsRequest: {:?}", e);
                }
            } else {
                error!("ServerEvent sender not available for chunk continuation");
            }
        }

        Ok(())
    }

    async fn handle_outgoing_request(
        &self,
        stream_id: u32,
        list_index: u32,
        position: u64,
        requested_size: u32,
        is_size_request: bool,
        server_event_sender: &mpsc::UnboundedSender<ServerEvent>,
    ) -> Result<()> {
        info!(
            "FileContentsRequest: stream={}, index={}, pos={}, size={}, size_req={}",
            stream_id, list_index, position, requested_size, is_size_request
        );

        let state = self.state.read();
        let file_info = state
            .outgoing_files
            .get(list_index as usize)
            .ok_or_else(|| {
                error!(
                    "Invalid file list index: {} (have {} files)",
                    list_index,
                    state.outgoing_files.len()
                );
                ClipboardError::InvalidState(format!("File index {list_index} not found"))
            })?;

        if is_size_request {
            info!(
                "Returning file size: {} bytes for '{}'",
                file_info.size, file_info.filename
            );

            let response = FileContentsResponse::new_size_response(stream_id, file_info.size);
            if let Err(e) = server_event_sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendFileContentsResponse(response),
            )) {
                error!("Failed to send FileContentsResponse: {:?}", e);
            }
        } else {
            let path = file_info.path.clone();
            let file_size = file_info.size;
            drop(state);

            match Self::read_file_chunk(&path, position, requested_size) {
                Ok(data) => {
                    info!(
                        "Read {} bytes from '{}' at offset {} (file size: {})",
                        data.len(),
                        path.display(),
                        position,
                        file_size
                    );

                    let response = FileContentsResponse::new_data_response(stream_id, data.clone());
                    if let Err(e) = server_event_sender.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    )) {
                        error!("Failed to send FileContentsResponse: {:?}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to read file '{}': {}", path.display(), e);
                    let response = FileContentsResponse::new_error(stream_id);
                    if let Err(e) = server_event_sender.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendFileContentsResponse(response),
                    )) {
                        error!("Failed to send FileContentsResponse error: {:?}", e);
                    }
                }
            }
        }

        Ok(())
    }

    fn set_outgoing_files(&self, files: Vec<OutgoingFileInfo>) {
        let mut state = self.state.write();
        state.outgoing_files = files;
        info!(
            "Set {} outgoing file(s) for Linux → Windows transfer",
            state.outgoing_files.len()
        );
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<FileTransferEvent> {
        #[expect(
            clippy::expect_used,
            reason = "single-subscribe contract; calling twice is a programmer error"
        )]
        self.event_rx
            .lock()
            .take()
            .expect("subscribe() called more than once")
    }

    fn allocate_stream_id(&self) -> u32 {
        self.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn health_check(&self) -> Result<()> {
        let state = self.state.read();
        if !state.download_dir.exists() {
            return Err(ClipboardError::FileTransferError(format!(
                "Download directory does not exist: {}",
                state.download_dir.display()
            )));
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        info!("Staging file transfer shutting down");
        let mut state = self.state.write();

        // Clean up any in-progress temp files
        for file in state.incoming_files.values() {
            info!("Cleaning up incomplete transfer: {}", file.filename);
            let _ = std::fs::remove_file(&file.temp_path);
        }
        state.incoming_files.clear();
        state.portal_serial = None;
        state.completed_files.clear();
        state.server_event_sender = None;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_staging_new() {
        let backend = StagingFileTransfer::new(PathBuf::from("/tmp/test-staging"));
        assert_eq!(backend.name(), "Staging Download");
        assert!(backend.requires_eager_download());
    }

    #[test]
    fn test_allocate_stream_id_increments() {
        let backend = StagingFileTransfer::new(PathBuf::from("/tmp/test"));
        let id1 = backend.allocate_stream_id();
        let id2 = backend.allocate_stream_id();
        assert_eq!(id2, id1 + 1);
    }

    fn descriptor(filename: &str, size: u64, file_index: u32) -> TransferFileDescriptor {
        TransferFileDescriptor {
            filename: filename.to_string(),
            size,
            file_index,
            clip_data_id: 1,
            relative_path: PathBuf::from(filename),
            is_directory: false,
            top_level: true,
        }
    }

    #[tokio::test]
    async fn test_zero_byte_entries_are_staged_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StagingFileTransfer::new(dir.path().to_path_buf());
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = backend
            .prepare_files(&[descriptor("empty.dat", 0, 0)], 7, &tx)
            .await
            .unwrap();

        match result {
            PrepareResult::Ready(paths) => assert_eq!(paths, vec![dir.path().join("empty.dat")]),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert_eq!(std::fs::metadata(dir.path().join("empty.dat")).unwrap().len(), 0);
        assert!(rx.try_recv().is_err(), "no FileContentsRequest for a zero-byte entry");
    }

    #[tokio::test]
    async fn test_mixed_paste_requests_only_sized_entries() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StagingFileTransfer::new(dir.path().to_path_buf());
        let (tx, mut rx) = mpsc::unbounded_channel();

        let result = backend
            .prepare_files(&[descriptor("dir", 0, 0), descriptor("a.txt", 5, 1)], 7, &tx)
            .await
            .unwrap();

        assert!(matches!(result, PrepareResult::Pending));
        match rx.try_recv().unwrap() {
            ServerEvent::Clipboard(ClipboardMessage::SendFileContentsRequest(req)) => {
                assert_eq!(req.index, 1);
                assert_eq!(req.requested_size, 5);
            }
            _ => panic!("expected a FileContentsRequest"),
        }
        assert!(rx.try_recv().is_err(), "exactly one request");

        backend.deliver_file_data(1, b"hello".to_vec(), false).await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"hello");
        assert!(dir.path().join("dir").exists());
    }

    #[tokio::test]
    async fn test_copied_directory_is_rebuilt_and_announced_once() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StagingFileTransfer::new(dir.path().to_path_buf());
        let mut events = backend.subscribe();
        let (tx, _rx) = mpsc::unbounded_channel();
        let nested = |name: &str, path: &str, size: u64, idx: u32, is_dir: bool| {
            TransferFileDescriptor {
                relative_path: PathBuf::from(path),
                is_directory: is_dir,
                top_level: !path.contains('/'),
                ..descriptor(name, size, idx)
            }
        };

        let result = backend
            .prepare_files(
                &[
                    nested("dir one", "dir one", 0, 0, true),
                    nested("sub", "dir one/sub", 0, 1, true),
                    nested("nested.txt", "dir one/sub/nested.txt", 5, 2, false),
                ],
                7,
                &tx,
            )
            .await
            .unwrap();
        assert!(matches!(result, PrepareResult::Pending));

        backend.deliver_file_data(1, b"hello".to_vec(), false).await.unwrap();

        assert_eq!(
            std::fs::read(dir.path().join("dir one/sub/nested.txt")).unwrap(),
            b"hello"
        );
        match events.try_recv().unwrap() {
            FileTransferEvent::FilesReady { paths, portal_serial } => {
                assert_eq!(portal_serial, 7);
                assert_eq!(paths, vec![dir.path().join("dir one")], "only the copied item");
            }
            other => panic!("expected FilesReady, got {other:?}"),
        }
    }
}
