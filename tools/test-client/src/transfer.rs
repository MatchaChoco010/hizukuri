//! What this client's selection sources hand out, and how it is written:
//! without blocking, a piece at every pump, and not at all while the test
//! holds the transfers back. A large selection does not stall the client
//! (or the test, which pumps every client on one thread) while the reader
//! is slow, and a test can keep a transfer open on purpose.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;

use crate::{ClientError, State, TestClient};

/// What a source hands out for each type it offers.
#[derive(Debug, Clone)]
pub(crate) enum Payload {
    /// The same text, whatever type is asked for.
    Same(String),
    /// Contents per type; a type without any reads as empty.
    PerType(Vec<(String, Vec<u8>)>),
}

impl Payload {
    pub(crate) fn per_type(contents: &[(&str, &[u8])]) -> Self {
        Payload::PerType(
            contents
                .iter()
                .map(|(mime_type, bytes)| (mime_type.to_string(), bytes.to_vec()))
                .collect(),
        )
    }

    pub(crate) fn mime_types(&self) -> Vec<String> {
        match self {
            Payload::Same(_) => Vec::new(),
            Payload::PerType(contents) => contents.iter().map(|(t, _)| t.clone()).collect(),
        }
    }

    pub(crate) fn for_type(&self, mime_type: &str) -> Vec<u8> {
        match self {
            Payload::Same(text) => text.as_bytes().to_vec(),
            Payload::PerType(contents) => contents
                .iter()
                .find(|(t, _)| t == mime_type)
                .map(|(_, bytes)| bytes.clone())
                .unwrap_or_default(),
        }
    }
}

struct Write_ {
    pipe: File,
    bytes: Vec<u8>,
    done: usize,
}

/// Transfers this client is writing.
#[derive(Default)]
pub(crate) struct Writes {
    pending: Vec<Write_>,
    held: bool,
}

impl State {
    /// Starts writing `bytes` into the write end of a transfer; closed when
    /// all of it is written (or the reader went away).
    pub(crate) fn start_write(&mut self, fd: OwnedFd, bytes: Vec<u8>) {
        if rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK).is_err() {
            // Blocking then; small texts fit into the pipe.
            let _ = File::from(fd).write_all(&bytes);
            return;
        }
        self.data.writes.pending.push(Write_ {
            pipe: File::from(fd),
            bytes,
            done: 0,
        });
        self.advance_writes();
    }

    /// Writes what the pipes take now.
    pub(crate) fn advance_writes(&mut self) {
        let writes = &mut self.data.writes;
        if writes.held {
            return;
        }
        writes.pending.retain_mut(|write| {
            while write.done < write.bytes.len() {
                match write.pipe.write(&write.bytes[write.done..]) {
                    Ok(0) => return false,
                    Ok(count) => write.done += count,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    // The reader went away.
                    Err(_) => return false,
                }
            }
            false
        });
    }
}

impl TestClient {
    /// Like [`TestClient::set_selection`], with contents of its own for
    /// each type (offered in this order).
    pub fn set_selection_types(&mut self, contents: &[(&str, &[u8])]) -> Result<(), ClientError> {
        self.set_selection_payload(Payload::per_type(contents))
    }

    /// Like [`TestClient::set_primary_selection`], with contents of its own
    /// for each type (offered in this order).
    pub fn set_primary_selection_types(
        &mut self,
        contents: &[(&str, &[u8])],
    ) -> Result<(), ClientError> {
        self.set_primary_selection_payload(Payload::per_type(contents))
    }

    /// Holds back (`true`) or resumes the writing of what this client's
    /// sources were asked for. Held, the readers' pipes stay open and
    /// empty.
    pub fn hold_transfers(&mut self, held: bool) {
        self.state.data.writes.held = held;
        self.state.advance_writes();
    }

    /// How many transfers this client has not finished writing.
    pub fn transfers_in_progress(&self) -> usize {
        self.state.data.writes.pending.len()
    }

    /// Stops reading the selections it was asked for: nothing more arrives
    /// as [`crate::ClientEvent::Received`] until it resumes. A writer on the
    /// other side then waits on a full pipe.
    pub fn hold_reads(&mut self, held: bool) {
        self.state.data.reads_held = held;
    }

    /// Bytes read so far of the transfers not yet complete.
    pub fn bytes_being_read(&self) -> usize {
        self.state.data.bytes_being_read()
    }
}
