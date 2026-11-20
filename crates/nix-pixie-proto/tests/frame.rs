//! Tests for `nix_pixie_proto::frame`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests fail by panicking"
)]

use nix_pixie_proto::frame::*;

#[tokio::test]
async fn round_trip_and_limit() {
    let (mut a, mut b) = tokio::io::duplex(1024);
    write_frame(&mut a, &("hello", 42u32)).await.unwrap();
    let got: (String, u32) = read_frame(&mut b, 64).await.unwrap();
    assert_eq!(got, ("hello".to_string(), 42));

    write_frame(&mut a, &vec![0u8; 100]).await.unwrap();
    let err = read_frame::<_, Vec<u8>>(&mut b, 64).await.unwrap_err();
    assert!(
        matches!(err, FrameError::TooLarge { limit: 64, .. }),
        "{err}"
    );
}
