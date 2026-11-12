use super::super::{Message, TurnSubmission};
use super::*;
use crate::runtime::session::TurnCompletionFence;
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn h1_owned_cancel_retry_retains_unfinished_tail_receipt() {
    let control = OwnedControl::new();
    let (send, receiver) = mpsc::unbounded_channel();
    let (consumed, barrier) = oneshot::channel();
    let (result, receipt) = oneshot::channel();
    let mut stream = TurnSubmission {
        owned: Some(control.clone()),
        receiver,
        consumed: Some(consumed),
        result: Some(receipt),
    };
    assert!(send.send(Message::End).is_ok());
    drop(send);
    let mut first = Box::pin(stream.cancel_and_drain("stop".into()));
    std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    barrier.await.unwrap();
    drop(first);
    let mut retry = Box::pin(stream.cancel_and_drain("second stop".into()));
    std::future::poll_fn(|cx| {
        assert!(
            retry.as_mut().poll(cx).is_pending(),
            "EOF is not the cleanup receipt"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(control.reason(), Some("stop".into()));
    result.send(control.finish(Ok(()))).unwrap();
    retry.await.unwrap();
}

#[tokio::test]
async fn h1_owned_cancel_targets_captured_completion_after_same_id_reuse() {
    let fences = TurnCompletionFence::default();
    let key = "same-session".into();
    let run = "same-run".into();
    let old = fences.begin(&key, &run);
    let control = OwnedControl::new();
    control.admitted(old.clone());
    let successor = fences.begin(&key, &run);
    control.cancel("old stopped".into());
    assert!(old.is_cancelled());
    assert!(!successor.is_cancelled());
    fences.complete(&key, &old);
    assert!(fences.is_current(&key, &successor));
    control.finish(Ok(())).unwrap();
    control.cancel("late".into());
    assert!(!successor.is_cancelled());
    fences.complete(&key, &successor);
}

#[tokio::test]
async fn h1_owned_cancel_before_admission_marks_only_its_later_completion() {
    let control = OwnedControl::new();
    control.cancel("stop before model".into());
    let fences = TurnCompletionFence::default();
    let own = fences.begin(&"session".into(), &"run".into());
    control.admitted(own.clone());
    assert!(own.is_cancelled());
    let other = fences.begin(&"other".into(), &"run".into());
    assert!(!other.is_cancelled());
}
