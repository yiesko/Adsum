use super::*;

#[derive(Debug)]
enum ReadStep {
  Bytes(Vec<u8>),
  Error(io::ErrorKind),
}

struct Reader(VecDeque<ReadStep>);

impl Read for Reader {
  fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
    match self.0.pop_front() {
      Some(ReadStep::Bytes(mut bytes)) => {
        let n = output.len().min(bytes.len());
        output[..n].copy_from_slice(&bytes[..n]);
        bytes.drain(..n);
        if !bytes.is_empty() {
          self.0.push_front(ReadStep::Bytes(bytes));
        }
        Ok(n)
      }
      Some(ReadStep::Error(kind)) => Err(io::Error::from(kind)),
      None => Ok(0),
    }
  }
}

#[test]
fn every_header_payload_and_utf8_split_survives_retryable_reads() {
  let message = "partial UTF-8: olá 🦀";
  let bytes = encode(PacketType::Frame, message);
  for split in 1..bytes.len() {
    let mut reader = Reader(VecDeque::from([
      ReadStep::Bytes(bytes[..split].to_vec()),
      ReadStep::Error(io::ErrorKind::WouldBlock),
      ReadStep::Error(io::ErrorKind::TimedOut),
      ReadStep::Error(io::ErrorKind::Interrupted),
      ReadStep::Bytes(bytes[split..].to_vec()),
    ]));
    let mut decoder = Decoder::default();
    let mut received = false;
    for _ in 0..12 {
      match decoder.poll_read(&mut reader).expect("valid frame") {
        ReadFrame::Pending => {}
        ReadFrame::Frame(packet, body) => {
          assert_eq!(packet, PacketType::Frame, "split {split}");
          assert_eq!(body, message, "split {split}");
          received = true;
          break;
        }
        ReadFrame::Eof => panic!("lost a partial frame at split {split}"),
      }
    }
    assert!(received, "frame not completed at split {split}");
    assert!(matches!(decoder.poll_read(&mut reader), Ok(ReadFrame::Eof)));
  }
}

#[test]
fn decoder_does_not_consume_the_next_frames_header() {
  let mut bytes = encode(PacketType::Ping, "first");
  bytes.extend(encode(PacketType::Pong, ""));
  bytes.extend(encode(PacketType::Frame, "last"));
  let mut reader = io::Cursor::new(bytes);
  let mut decoder = Decoder::default();
  for (expected_packet, expected_message) in [
    (PacketType::Ping, "first"),
    (PacketType::Pong, ""),
    (PacketType::Frame, "last"),
  ] {
    let ReadFrame::Frame(packet, mut message) = decoder.poll_read(&mut reader).unwrap() else {
      panic!("complete frame expected");
    };
    assert_eq!(packet, expected_packet);
    assert_eq!(message, expected_message);
    message.clear();
    decoder.recycle(message);
  }
  assert!(matches!(decoder.poll_read(&mut reader), Ok(ReadFrame::Eof)));
}

#[test]
fn eof_in_a_header_or_body_is_not_a_complete_frame() {
  let frame = encode(PacketType::Frame, "payload");
  for split in 1..frame.len() {
    let mut decoder = Decoder::default();
    let mut reader = io::Cursor::new(&frame[..split]);
    let mut truncated = false;
    for _ in 0..3 {
      match decoder.poll_read(&mut reader) {
        Ok(ReadFrame::Pending) => {}
        Err(DecodeError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
          truncated = true;
          break;
        }
        _ => panic!("partial frame accepted at split {split}"),
      }
    }
    assert!(truncated, "missing EOF at split {split}");
  }
}

#[test]
fn oversized_and_unknown_headers_are_rejected_before_allocating_a_body() {
  for (opcode, size) in [(1_u32, MAX_IPC_PAYLOAD + 1), (99, MAX_IPC_PAYLOAD)] {
    let mut header = opcode.to_le_bytes().to_vec();
    header.extend(size.to_le_bytes());
    let mut decoder = Decoder::default();
    let result = decoder.poll_read(&mut io::Cursor::new(header));
    if opcode == 99 {
      assert!(matches!(result, Err(DecodeError::UnsupportedPacket)));
    } else {
      assert!(matches!(result, Err(DecodeError::TooLarge)));
    }
    assert_eq!(decoder.payload.capacity(), 0);
  }
}

#[test]
fn invalid_utf8_is_rejected_only_after_the_whole_body_arrives() {
  let mut bytes = encode(PacketType::Frame, "xx");
  bytes[8..].copy_from_slice(&[0xff, 0xff]);
  let mut decoder = Decoder::default();
  assert!(matches!(
    decoder.poll_read(&mut io::Cursor::new(bytes)),
    Err(DecodeError::InvalidUtf8)
  ));
}

struct Writer {
  steps: VecDeque<io::Result<usize>>,
  bytes: Vec<u8>,
}

impl Write for Writer {
  fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    let n = self
      .steps
      .pop_front()
      .unwrap_or(Ok(usize::MAX))?
      .min(bytes.len());
    self.bytes.extend_from_slice(&bytes[..n]);
    Ok(n)
  }

  fn flush(&mut self) -> io::Result<()> {
    panic!("named-pipe flush must not block the pump")
  }
}

#[test]
fn outgoing_header_and_body_offsets_survive_partial_writes_and_timeouts() {
  let mut replies = Replies::default();
  replies.push(PacketType::Pong, "first reply").unwrap();
  replies.push(PacketType::Frame, "second reply").unwrap();
  let mut writer = Writer {
    steps: VecDeque::from([
      Ok(3),
      Err(io::ErrorKind::WouldBlock.into()),
      Ok(6),
      Err(io::ErrorKind::TimedOut.into()),
      Ok(2),
      Err(io::ErrorKind::Interrupted.into()),
      Ok(usize::MAX),
    ]),
    bytes: Vec::new(),
  };
  for _ in 0..12 {
    replies.poll_flush(&mut writer).unwrap();
    if replies.is_empty() {
      break;
    }
  }
  assert!(replies.is_empty());
  assert_eq!(replies.bytes, 0);
  let mut expected = encode(PacketType::Pong, "first reply");
  expected.extend(encode(PacketType::Frame, "second reply"));
  assert_eq!(writer.bytes, expected);
}

#[test]
fn outgoing_queue_is_bounded_and_stalled_peers_are_closed() {
  let mut replies = Replies::default();
  for _ in 0..MAX_QUEUED_REPLIES {
    replies.push(PacketType::Pong, "queued").unwrap();
  }
  assert!(replies.push(PacketType::Pong, "overflow").is_err());
  replies.last_progress = Some(Instant::now() - WRITE_STALL_TIMEOUT);
  let mut writer = io::sink();
  assert_eq!(
    replies.poll_flush(&mut writer).unwrap_err().kind(),
    io::ErrorKind::TimedOut
  );
}

#[test]
fn write_zero_and_broken_pipe_are_fatal_instead_of_dropping_a_reply() {
  for step in [Ok(0), Err(io::ErrorKind::BrokenPipe.into())] {
    let mut replies = Replies::default();
    replies.push(PacketType::Frame, "reply").unwrap();
    let mut writer = Writer {
      steps: VecDeque::from([step]),
      bytes: Vec::new(),
    };
    assert!(replies.poll_flush(&mut writer).is_err());
    assert!(!replies.is_empty());
  }
}
