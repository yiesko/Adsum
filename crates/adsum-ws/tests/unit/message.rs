use super::*;

/// Binary clones share the backing (pointer-equal), never copy bytes.
#[test]
fn binary_clone_shares_allocation() {
  let msg = Message::Binary(Bytes::from(vec![1u8; 1024]));
  let cloned = msg.clone();
  let (Message::Binary(a), Message::Binary(b)) = (msg, cloned) else {
    panic!("expected Binary variants");
  };
  assert!(!a.is_empty());
  assert_eq!(a.as_ptr(), b.as_ptr());
}

/// Text clones share the backing (pointer-equal), never copy bytes.
#[test]
fn text_clone_shares_allocation() {
  let msg = Message::from(String::from("hello"));
  let cloned = msg.clone();
  let (Message::Text(a), Message::Text(b)) = (msg, cloned) else {
    panic!("expected Text variants");
  };
  assert_eq!(a.as_str(), "hello");
  assert!(std::ptr::eq(a.as_str().as_ptr(), b.as_str().as_ptr()));
}

/// Every `From` direction builds the expected variant with equal bytes.
#[test]
fn conversions_cover_common_inputs() {
  assert_eq!(
    Message::from("hi"),
    Message::from(Utf8Bytes::from_static("hi"))
  );
  assert_eq!(
    Message::from(String::from("hi")),
    Message::from(Utf8Bytes::from_static("hi"))
  );
  assert_eq!(
    Message::from(vec![1u8, 2]),
    Message::Binary(Bytes::from_static(&[1, 2]))
  );
  assert!(Message::from("").is_empty());
  assert_eq!(Message::from("hi").len(), 2);
}
