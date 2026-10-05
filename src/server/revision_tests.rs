use super::{DocumentRevisionSync, RevisionError, RevisionTicket, wait_for_captured_revision};
use crate::{analysis::Revision, workspace::Document};
use std::{
    error::Error,
    future::{Future, poll_fn},
    io,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};
use url::Url;

#[test]
fn closed_epoch_cannot_repopulate_reopened_staging_cache() -> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let old = sync
        .reserve(Revision(7))?
        .ok_or_else(|| io::Error::other("reserve old epoch"))?;
    let old_document = Document::new(
        Url::parse("file:///revision-epoch.bend")?,
        "bend".to_owned(),
        Revision(7),
        "def old = 7\n".to_owned(),
    );

    sync.close()?;
    let reopened = sync
        .reserve(Revision(1))?
        .ok_or_else(|| io::Error::other("reserve reopened epoch"))?;
    assert!(!old.store_staged_document(old_document)?);

    assert!(reopened.staged_document()?.is_none());
    assert!(!old.mark_committed()?);
    drop(old);
    assert!(reopened.is_current()?);
    assert!(reopened.mark_committed()?);
    Ok(())
}

fn reserve(
    sync: &Arc<DocumentRevisionSync>,
    revision: i32,
) -> Result<RevisionTicket, Box<dyn Error>> {
    sync.reserve(Revision(revision))?
        .ok_or_else(|| io::Error::other("revision reservation was rejected").into())
}

fn document(revision: i32, text: &str) -> Result<Document, Box<dyn Error>> {
    Ok(Document::new(
        Url::parse("file:///revision-order.bend")?,
        "bend".to_owned(),
        Revision(revision),
        text.to_owned(),
    ))
}

async fn is_pending<F: Future>(mut future: Pin<&mut F>) -> bool {
    poll_fn(|context| Poll::Ready(future.as_mut().poll(context).is_pending())).await
}

#[tokio::test]
async fn superseded_staging_remains_ordered_and_supplies_the_next_edit()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let first = reserve(&sync, 1)?;
    let second = reserve(&sync, 2)?;
    assert!(first.wait_for_turn().await?);
    let second_turn = second.wait_for_turn();
    tokio::pin!(second_turn);
    assert!(is_pending(second_turn.as_mut()).await);

    assert!(first.store_staged_document(document(1, "def first = 1\n")?)?);
    assert!(!first.mark_committed()?);
    drop(first);
    assert!(tokio::time::timeout(Duration::from_secs(1), second_turn).await??);
    let inherited = second
        .staged_document()?
        .ok_or_else(|| io::Error::other("missing ordered staged snapshot"))?;
    assert_eq!(inherited.revision, Revision(1));
    assert_eq!(inherited.text, "def first = 1\n");
    assert!(sync.status()?.is_pending());
    assert!(!sync.status()?.is_failed());

    assert!(second.store_staged_document(document(2, "def second = 2\n")?)?);
    assert!(second.mark_committed()?);
    let status = sync.status()?;
    assert_eq!(status.desired(), Some(Revision(2)));
    assert_eq!(status.committed(), Some(Revision(2)));
    assert!(!status.is_pending());
    Ok(())
}

#[tokio::test]
async fn all_out_of_order_drop_permutations_release_the_next_generation()
-> Result<(), Box<dyn Error>> {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let sync = Arc::new(DocumentRevisionSync::new(None));
        let mut older = [
            Some(reserve(&sync, 1)?),
            Some(reserve(&sync, 2)?),
            Some(reserve(&sync, 3)?),
        ];
        let latest = reserve(&sync, 4)?;
        let turn = latest.wait_for_turn();
        tokio::pin!(turn);
        assert!(is_pending(turn.as_mut()).await);
        for index in order {
            let ticket = older[index]
                .take()
                .ok_or_else(|| io::Error::other("duplicate drop permutation index"))?;
            assert!(!ticket.mark_committed()?);
            drop(ticket);
            assert!(sync.status()?.is_pending());
            assert!(!sync.status()?.is_failed());
            assert_eq!(sync.status()?.desired(), Some(Revision(4)));
        }
        assert!(tokio::time::timeout(Duration::from_secs(1), turn).await??);
        assert!(latest.mark_committed()?);
        let status = sync.status()?;
        assert_eq!(status.committed(), Some(Revision(4)));
        assert_eq!(status.desired(), Some(Revision(4)));
    }
    Ok(())
}

#[tokio::test]
async fn dropping_current_ticket_releases_readiness_and_allows_a_new_revision()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let failed = reserve(&sync, 1)?;
    let readiness = wait_for_captured_revision(sync.subscribe(), failed.generation());
    tokio::pin!(readiness);
    assert!(is_pending(readiness.as_mut()).await);
    drop(failed);
    tokio::time::timeout(Duration::from_secs(1), readiness).await??;
    assert!(sync.status()?.is_failed());
    assert!(!sync.status()?.is_pending());
    assert_eq!(sync.status()?.committed(), None);

    let next = reserve(&sync, 2)?;
    assert!(sync.status()?.is_pending());
    assert!(!sync.status()?.is_failed());
    assert!(tokio::time::timeout(Duration::from_secs(1), next.wait_for_turn()).await??);
    assert!(next.mark_committed()?);
    assert_eq!(sync.status()?.committed(), Some(Revision(2)));
    Ok(())
}

#[tokio::test]
async fn captured_readiness_does_not_follow_a_newer_pending_generation()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let first = reserve(&sync, 1)?;
    let first_readiness = wait_for_captured_revision(sync.subscribe(), first.generation());
    tokio::pin!(first_readiness);
    assert!(is_pending(first_readiness.as_mut()).await);
    let second = reserve(&sync, 2)?;
    tokio::time::timeout(Duration::from_secs(1), first_readiness).await??;
    assert!(sync.status()?.is_pending());
    assert!(!first.mark_committed()?);
    drop(first);

    let second_readiness = wait_for_captured_revision(sync.subscribe(), second.generation());
    tokio::pin!(second_readiness);
    assert!(is_pending(second_readiness.as_mut()).await);
    assert!(second.mark_committed()?);
    tokio::time::timeout(Duration::from_secs(1), second_readiness).await??;
    assert_eq!(sync.status()?.committed(), Some(Revision(2)));
    Ok(())
}

#[tokio::test]
async fn close_wakes_old_epoch_waiters_without_blocking_or_failing_reopen()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let first = reserve(&sync, 7)?;
    let blocked = reserve(&sync, 8)?;
    {
        let blocked_turn = blocked.wait_for_turn();
        tokio::pin!(blocked_turn);
        let readiness = wait_for_captured_revision(sync.subscribe(), blocked.generation());
        tokio::pin!(readiness);
        assert!(is_pending(blocked_turn.as_mut()).await);
        assert!(is_pending(readiness.as_mut()).await);

        sync.close()?;
        assert!(!tokio::time::timeout(Duration::from_secs(1), blocked_turn).await??);
        tokio::time::timeout(Duration::from_secs(1), readiness).await??;
    }
    assert_eq!(sync.status()?.desired(), None);
    assert_eq!(sync.status()?.committed(), None);

    let reopened = reserve(&sync, 1)?;
    assert!(tokio::time::timeout(Duration::from_secs(1), reopened.wait_for_turn()).await??);
    assert!(reopened.store_staged_document(document(1, "def reopened = 1\n")?)?);
    assert!(!first.store_staged_document(document(7, "def stale = 7\n")?)?);
    assert!(!blocked.mark_committed()?);
    drop(first);
    drop(blocked);
    assert!(reopened.is_current()?);
    assert!(sync.status()?.is_pending());
    assert!(!sync.status()?.is_failed());
    let staged = reopened
        .staged_document()?
        .ok_or_else(|| io::Error::other("missing reopened snapshot"))?;
    assert_eq!(staged.text, "def reopened = 1\n");
    assert!(reopened.mark_committed()?);
    assert_eq!(sync.status()?.committed(), Some(Revision(1)));
    Ok(())
}

#[test]
fn rejected_reservations_and_stale_tickets_do_not_publish_status_changes()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let first = reserve(&sync, 1)?;
    let mut status = sync.subscribe();
    assert!(sync.reserve(Revision(1))?.is_none());
    assert!(sync.reserve(Revision(0))?.is_none());
    assert!(sync.reserve_refresh(Revision(2))?.is_none());
    assert!(!status.has_changed()?);

    let second = reserve(&sync, 2)?;
    assert!(status.has_changed()?);
    assert_eq!(status.borrow_and_update().generation(), second.generation());
    assert!(!first.mark_committed()?);
    drop(first);
    assert!(!status.has_changed()?);
    assert!(second.mark_committed()?);
    assert!(status.has_changed()?);
    assert_eq!(status.borrow_and_update().committed(), Some(Revision(2)));
    assert!(second.mark_committed()?);
    drop(second);
    assert!(!status.has_changed()?);
    Ok(())
}

#[tokio::test]
async fn refresh_of_same_revision_has_independent_commit_and_failure_generation()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(Some(document(
        5,
        "def current = 5\n",
    )?)));
    assert!(!sync.status()?.is_pending());
    assert!(sync.reserve(Revision(5))?.is_none());
    assert!(sync.reserve_refresh(Revision(6))?.is_none());
    let first = sync
        .reserve_refresh(Revision(5))?
        .ok_or_else(|| io::Error::other("reserve first refresh"))?;
    assert!(sync.status()?.is_pending());
    assert_eq!(sync.status()?.committed(), None);
    let second = sync
        .reserve_refresh(Revision(5))?
        .ok_or_else(|| io::Error::other("reserve second refresh"))?;
    assert!(!first.mark_committed()?);
    drop(first);
    assert!(!sync.status()?.is_failed());
    assert!(tokio::time::timeout(Duration::from_secs(1), second.wait_for_turn()).await??);
    drop(second);
    assert!(sync.status()?.is_failed());
    assert_eq!(sync.status()?.desired(), Some(Revision(5)));
    assert_eq!(sync.status()?.committed(), None);

    let retry = sync
        .reserve_refresh(Revision(5))?
        .ok_or_else(|| io::Error::other("reserve retry refresh"))?;
    assert!(sync.status()?.is_pending());
    assert!(!sync.status()?.is_failed());
    assert!(tokio::time::timeout(Duration::from_secs(1), retry.wait_for_turn()).await??);
    assert!(retry.mark_committed()?);
    assert_eq!(sync.status()?.committed(), Some(Revision(5)));
    Ok(())
}

#[test]
fn failed_newer_revision_retains_the_last_committed_revision() -> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let first = reserve(&sync, 1)?;
    assert!(first.mark_committed()?);
    drop(first);
    let second = reserve(&sync, 2)?;
    assert_eq!(sync.status()?.desired(), Some(Revision(2)));
    assert_eq!(sync.status()?.committed(), Some(Revision(1)));
    assert!(sync.status()?.is_pending());

    let third = reserve(&sync, 3)?;
    assert!(!second.mark_committed()?);
    drop(second);
    assert_eq!(sync.status()?.desired(), Some(Revision(3)));
    assert_eq!(sync.status()?.committed(), Some(Revision(1)));
    assert!(sync.status()?.is_pending());
    drop(third);
    assert_eq!(sync.status()?.desired(), Some(Revision(3)));
    assert_eq!(sync.status()?.committed(), Some(Revision(1)));
    assert!(sync.status()?.is_failed());
    assert!(!sync.status()?.is_pending());
    Ok(())
}

#[tokio::test]
async fn dropped_predecessor_wakes_staging_then_commit_wakes_captured_readiness()
-> Result<(), Box<dyn Error>> {
    let sync = Arc::new(DocumentRevisionSync::new(None));
    let first = reserve(&sync, 1)?;
    let second = reserve(&sync, 2)?;
    let readiness = wait_for_captured_revision(sync.subscribe(), second.generation());
    let (stage_started, stage_waiting) = tokio::sync::oneshot::channel();
    let stage = tokio::spawn(async move {
        let ready = {
            let turn = second.wait_for_turn();
            tokio::pin!(turn);
            let pending = is_pending(turn.as_mut()).await;
            let _sent = stage_started.send(pending);
            turn.await?
        };
        if ready {
            second.mark_committed()?;
        }
        Ok::<_, RevisionError>(ready)
    });
    let (read_started, read_waiting) = tokio::sync::oneshot::channel();
    let read = tokio::spawn(async move {
        tokio::pin!(readiness);
        let pending = is_pending(readiness.as_mut()).await;
        let _sent = read_started.send(pending);
        readiness.await
    });
    assert!(tokio::time::timeout(Duration::from_secs(1), stage_waiting).await??);
    assert!(tokio::time::timeout(Duration::from_secs(1), read_waiting).await??);

    drop(first);
    assert!(tokio::time::timeout(Duration::from_secs(1), stage).await???);
    tokio::time::timeout(Duration::from_secs(1), read).await???;
    assert_eq!(sync.status()?.committed(), Some(Revision(2)));
    Ok(())
}
