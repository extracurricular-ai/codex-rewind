//! Verifies conservative classification at the shared storage error boundary.

use super::ThreadStoreError;
use pretty_assertions::assert_eq;

#[test]
fn unclassified_sqlx_failures_remain_internal() {
    for error in [
        sqlx::Error::PoolTimedOut,
        sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection lost after write",
        )),
        sqlx::Error::Protocol("retryable conflict; outcome unknown".to_owned()),
    ] {
        let expected = format!("failed to access thread history: {error}");
        let error = ThreadStoreError::from(error);
        let ThreadStoreError::Internal { message } = error else {
            panic!("unclassified SQLx failure must remain internal: {error:?}");
        };
        assert_eq!(message, expected);
    }
}
