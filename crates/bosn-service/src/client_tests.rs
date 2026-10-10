//! The CI client's refusal classification.

use super::*;

#[test]
fn only_a_definite_refusal_discards_the_staged_snapshot() {
    let refused: Result<(), Error> = Err(Error::Ci {
        code: "refused".into(),
        message: "no".into(),
    });
    assert!(daemon_refused(&refused));
    // The daemon may have accepted the run before the reply was lost.
    assert!(!daemon_refused::<()>(&Err(Error::Deadline)));
    assert!(!daemon_refused::<()>(&Err(Error::Protocol(
        "ci reply decode"
    ))));
    assert!(!daemon_refused(&Ok(())));
}
