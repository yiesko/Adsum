//! Incremental framing for nonblocking Unix sockets and Windows pipes.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use crate::frame::{MAX_IPC_PAYLOAD, PacketType, encode};

/// A slow reader cannot retain a committed session indefinitely.
const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_QUEUED_REPLIES: usize = 8;
const MAX_QUEUED_BYTES: usize = MAX_IPC_PAYLOAD as usize * 8;

#[derive(Default)]
pub(crate) struct Decoder {
  header: [u8; 8],
  header_len: usize,
  packet: Option<PacketType>,
  payload: Vec<u8>,
  payload_len: usize,
}

pub(crate) enum ReadFrame {
  Pending,
  Eof,
  Frame(PacketType, String),
}

#[derive(Debug)]
pub(crate) enum DecodeError {
  Io(io::Error),
  TooLarge,
  UnsupportedPacket,
  InvalidUtf8,
}

impl Decoder {
  /// At most two reads per poll, so even an active peer cannot starve
  /// cancellation or outgoing replies.
  pub(crate) fn poll_read(&mut self, stream: &mut impl Read) -> Result<ReadFrame, DecodeError> {
    if self.header_len < self.header.len() {
      match stream.read(&mut self.header[self.header_len..]) {
        Ok(0) if self.header_len == 0 => return Ok(ReadFrame::Eof),
        Ok(0) => return Err(truncated_frame()),
        Ok(n) => self.header_len += n,
        Err(error) if retryable(&error) => return Ok(ReadFrame::Pending),
        Err(error) => return Err(DecodeError::Io(error)),
      }
      if self.header_len < self.header.len() {
        return Ok(ReadFrame::Pending);
      }
    }
    if self.packet.is_none() {
      let opcode = u32::from_le_bytes(self.header[..4].try_into().expect("four-byte opcode"));
      let size = u32::from_le_bytes(self.header[4..].try_into().expect("four-byte length"));
      if size > MAX_IPC_PAYLOAD {
        return Err(DecodeError::TooLarge);
      }
      self.packet = Some(PacketType::try_from_u32(opcode).ok_or(DecodeError::UnsupportedPacket)?);
      self.payload.resize(size as usize, 0);
    }
    if self.payload_len < self.payload.len() {
      match stream.read(&mut self.payload[self.payload_len..]) {
        Ok(0) => return Err(truncated_frame()),
        Ok(n) => self.payload_len += n,
        Err(error) if retryable(&error) => return Ok(ReadFrame::Pending),
        Err(error) => return Err(DecodeError::Io(error)),
      }
      if self.payload_len < self.payload.len() {
        return Ok(ReadFrame::Pending);
      }
    }
    let message =
      String::from_utf8(std::mem::take(&mut self.payload)).map_err(|_| DecodeError::InvalidUtf8)?;
    let packet = self.packet.take().expect("complete frame has an opcode");
    self.header_len = 0;
    self.payload_len = 0;
    Ok(ReadFrame::Frame(packet, message))
  }

  pub(crate) fn recycle(&mut self, message: String) {
    self.payload = message.into_bytes();
  }
}

fn truncated_frame() -> DecodeError {
  DecodeError::Io(io::Error::new(
    io::ErrorKind::UnexpectedEof,
    "truncated IPC frame",
  ))
}

fn retryable(error: &io::Error) -> bool {
  matches!(
    error.kind(),
    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
  )
}

struct PendingWrite {
  bytes: Vec<u8>,
  offset: usize,
}

#[derive(Default)]
pub(crate) struct Replies {
  queue: VecDeque<PendingWrite>,
  bytes: usize,
  last_progress: Option<Instant>,
}

impl Replies {
  pub(crate) fn push(&mut self, packet: PacketType, message: &str) -> io::Result<()> {
    self.push_encoded(encode(packet, message))
  }

  pub(crate) fn push_encoded(&mut self, bytes: Vec<u8>) -> io::Result<()> {
    if self.queue.len() >= MAX_QUEUED_REPLIES
      || bytes.len() > MAX_QUEUED_BYTES.saturating_sub(self.bytes)
    {
      return Err(io::Error::new(
        io::ErrorKind::OutOfMemory,
        "IPC reply queue full",
      ));
    }
    if self.queue.is_empty() {
      self.last_progress = Some(Instant::now());
    }
    self.bytes += bytes.len();
    self.queue.push_back(PendingWrite { bytes, offset: 0 });
    Ok(())
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.queue.is_empty()
  }

  /// Preserve partially written headers and payloads on WouldBlock/TimedOut.
  /// Do not call Write::flush: named-pipe flush may wait for the peer.
  pub(crate) fn poll_flush(&mut self, stream: &mut impl Write) -> io::Result<bool> {
    let Some(front) = self.queue.front_mut() else {
      return Ok(false);
    };
    if self
      .last_progress
      .is_some_and(|last| last.elapsed() >= WRITE_STALL_TIMEOUT)
    {
      return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC reply stalled"));
    }
    match stream.write(&front.bytes[front.offset..]) {
      Ok(0) => Err(io::Error::new(
        io::ErrorKind::WriteZero,
        "IPC peer stopped reading",
      )),
      Ok(n) => {
        front.offset += n;
        self.last_progress = Some(Instant::now());
        if front.offset == front.bytes.len() {
          self.bytes -= front.bytes.len();
          self.queue.pop_front();
        }
        Ok(true)
      }
      Err(error) if retryable(&error) => Ok(false),
      Err(error) => Err(error),
    }
  }
}

#[cfg(test)]
#[path = "../tests/unit/codec.rs"]
mod tests;
