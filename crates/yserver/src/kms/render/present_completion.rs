//! Deferred PRESENT completion queue (Stage 5 Task 6.1).
//!
//! Owns per-entry state for the v2 backend's `enqueue_present_completion`
//! and `drain_completed_present_events` trait impls. Internal types
//! never escape the `yserver` crate; the trait surface exchanges
//! the public `CompletedPresentEvent` only.
//!
//! Spec: `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`.

use std::{
    os::fd::{AsFd, OwnedFd},
    sync::{Arc, Once},
};

use yserver_core::backend::{CompletedPresentEvent, SyncobjHandle, XshmfenceHandle};

use crate::kms::render::present_batch::PresentCompletionEntry;

static RELEASE_FENCE_PUBLISHED_LOG: Once = Once::new();

/// One deferred PRESENT completion payload. The drain fires the
/// wake signal via `wake_pin` + returns the `event` payload to the
/// main loop.
#[derive(Debug)]
pub(crate) struct PendingPresentEntry {
    /// Lifetime pin on the underlying wake primitive. Survives a
    /// mid-flight `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) wake_pin: PinnedWake,
    /// Public-facing event payload, returned by `drain_*` to the
    /// main loop.
    pub(crate) event: CompletedPresentEvent,
}

impl PresentCompletionEntry for PendingPresentEntry {
    /// Publish this entry's GPU completion fence into a synced Present's
    /// release timeline. Returns `true` when a release point was published,
    /// `false` for legacy/no-wake entries.
    ///
    /// State changes only after a successful import. On failure the original
    /// `PixmapSynced` wake remains intact, so completion falls back to the
    /// existing host-signal path rather than stranding the client buffer.
    fn publish_release_fence(&mut self, fence: &OwnedFd) -> std::io::Result<bool> {
        let PinnedWake::PixmapSynced { handle, value } = &self.wake_pin else {
            return Ok(false);
        };
        let handle = handle.clone();
        let value = *value;
        handle.import_sync_file(value, fence.as_fd())?;
        self.wake_pin = PinnedWake::PixmapSyncedFencePublished { handle, value };
        RELEASE_FENCE_PUBLISHED_LOG.call_once(|| {
            log::info!(
                "PresentPixmapSynced release points now carry submitted GPU completion fences"
            );
        });
        Ok(true)
    }
}

/// Recover the concrete entry from the renderer's opaque box.
///
/// # Panics
///
/// If `entry` is not a [`PendingPresentEntry`]. It is the only implementor,
/// so a mismatch is a bug, never a payload to drop.
pub(crate) fn into_pending_entry(entry: Box<dyn PresentCompletionEntry>) -> PendingPresentEntry {
    let any: Box<dyn std::any::Any> = entry;
    *any.downcast::<PendingPresentEntry>().expect(
        "PresentCompletionEntry is implemented only by PendingPresentEntry; \
         a different type reached the KMS present-completion drain",
    )
}

/// Wake-target lifetime pin variants. The drain dispatches signal
/// via the held `Arc` regardless of whether the X11 resource id is
/// still in the registry.
#[derive(Debug)]
pub(crate) enum PinnedWake {
    Pixmap(Arc<dyn XshmfenceHandle>),
    PixmapSynced {
        handle: Arc<dyn SyncobjHandle>,
        value: u64,
    },
    /// The completion fence has already been transferred into this release
    /// timeline point. Retain the handle through Present completion, but do
    /// not host-signal it again.
    PixmapSyncedFencePublished {
        handle: Arc<dyn SyncobjHandle>,
        value: u64,
    },
    /// Client passed no wake object (idle_fence_xid == 0 or
    /// release_syncobj == 0). Drain skips the signal step; X11 event
    /// emission still happens.
    None,
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use nix::sys::eventfd::{EfdFlags, EventFd};
    use yserver_core::backend::PresentWake;
    use yserver_protocol::x11::ClientId;

    #[derive(Debug)]
    struct TestSyncobj {
        imports: Mutex<Vec<u64>>,
        fail_import: bool,
    }

    impl SyncobjHandle for TestSyncobj {
        fn signal(&self, _value: u64) -> std::io::Result<()> {
            Ok(())
        }

        fn import_sync_file(
            &self,
            value: u64,
            _fd: std::os::fd::BorrowedFd<'_>,
        ) -> std::io::Result<()> {
            if self.fail_import {
                return Err(std::io::Error::other("injected import failure"));
            }
            self.imports.lock().unwrap().push(value);
            Ok(())
        }
    }

    fn synced_entry(handle: Arc<dyn SyncobjHandle>) -> PendingPresentEntry {
        PendingPresentEntry {
            wake_pin: PinnedWake::PixmapSynced {
                handle: handle.clone(),
                value: 17,
            },
            event: CompletedPresentEvent {
                client_id: ClientId(7),
                serial: 42,
                host_xid: 0x100001,
                dst_host_xid: 0xE00001,
                options: 0,
                present_id: 9,
                window_generation: 0,
                crtc_id: 0,
                crtc_epoch: 0,
                msc_offset: 0,
                completion_clock: None,
                wake: PresentWake::PixmapSynced {
                    release: handle,
                    release_syncobj: 0x200001,
                    release_value: 17,
                },
                completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
                emit_idle: true,
            },
        }
    }

    /// A failed attach hands the boxed entry back, and the downcast helper
    /// recovers the same concrete payload: event fields and wake pin intact.
    #[test]
    fn failed_attach_round_trips_entry_through_downcast() {
        use crate::kms::render::{engine::RenderEngine, store::DrawableId};

        let handle: Arc<dyn SyncobjHandle> = Arc::new(TestSyncobj {
            imports: Mutex::new(Vec::new()),
            fail_import: false,
        });
        let mut engine = RenderEngine::stub();
        let entry: Box<dyn PresentCompletionEntry> = Box::new(synced_entry(handle.clone()));

        let returned = engine
            .attach_present_completion(DrawableId::for_tests(1), entry)
            .expect_err("a stub engine has no open frame to attach to");
        let entry = into_pending_entry(returned);

        assert_eq!(entry.event.client_id, ClientId(7));
        assert_eq!(entry.event.serial, 42);
        assert_eq!(entry.event.host_xid, 0x100001);
        assert_eq!(entry.event.dst_host_xid, 0xE00001);
        assert_eq!(entry.event.present_id, 9);
        assert!(matches!(
            entry.event.wake,
            PresentWake::PixmapSynced {
                release_syncobj: 0x200001,
                release_value: 17,
                ..
            }
        ));
        match entry.wake_pin {
            PinnedWake::PixmapSynced {
                handle: pinned,
                value,
            } => {
                assert_eq!(value, 17);
                assert!(Arc::ptr_eq(&pinned, &handle));
            }
            other => panic!("wake pin changed variant: {other:?}"),
        }
    }

    /// Smoke test that the types compile + can be constructed.
    /// Real semantics tested in `KmsBackend` integration tests.
    #[test]
    fn pinned_wake_none_constructs() {
        let pin = PinnedWake::None;
        match pin {
            PinnedWake::None => {}
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn completed_present_event_carries_payload() {
        let event = CompletedPresentEvent {
            client_id: ClientId(7),
            serial: 42,
            host_xid: 0x100001,
            dst_host_xid: 0xE00001,
            options: 0,
            present_id: 0,
            window_generation: 0,
            crtc_id: 0,
            crtc_epoch: 0,
            msc_offset: 0,
            completion_clock: None,
            wake: PresentWake::Pixmap {
                idle_fence_xid: 0xCC,
            },
            completion_mode: yserver_protocol::x11::present::COMPLETE_MODE_COPY,
            emit_idle: true,
        };
        assert_eq!(event.serial, 42);
    }

    #[test]
    fn publishing_release_fence_replaces_host_signal_wake() {
        let handle = Arc::new(TestSyncobj {
            imports: Mutex::new(Vec::new()),
            fail_import: false,
        });
        let mut entry = synced_entry(handle.clone());
        let fence: OwnedFd = EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC)
            .expect("eventfd")
            .into();

        assert!(entry.publish_release_fence(&fence).expect("publish"));
        assert_eq!(*handle.imports.lock().unwrap(), vec![17]);
        assert!(matches!(
            entry.wake_pin,
            PinnedWake::PixmapSyncedFencePublished { value: 17, .. }
        ));
    }

    #[test]
    fn failed_release_fence_publish_preserves_host_signal_fallback() {
        let handle = Arc::new(TestSyncobj {
            imports: Mutex::new(Vec::new()),
            fail_import: true,
        });
        let mut entry = synced_entry(handle);
        let fence: OwnedFd = EventFd::from_value_and_flags(0, EfdFlags::EFD_CLOEXEC)
            .expect("eventfd")
            .into();

        assert!(entry.publish_release_fence(&fence).is_err());
        assert!(matches!(
            entry.wake_pin,
            PinnedWake::PixmapSynced { value: 17, .. }
        ));
    }
}
