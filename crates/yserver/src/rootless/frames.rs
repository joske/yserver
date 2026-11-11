//! The presenter's pure model per top-level: host readiness (spec §4.5.1),
//! damage since the last delivered frame, pool fill plans (§4.5.2) and the
//! store snapshots held until delivery (§4.5).

use std::collections::VecDeque;

use yserver_core::rootless::Rect;

use super::host::FrameSeq;

/// Frame buffers per window, as Xwayland's window buffers
/// (`xwayland-window-buffers.c:137-158`).
pub(crate) const POOL: usize = 3;

/// Spec §4.5.1.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Readiness {
    Setup,
    ReadyFirst,
    Busy,
    Ready,
}

/// What a `Delivered` means for the presenter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Delivery {
    /// The window's first delivered frame: make it visible.
    pub first: bool,
    /// Ack the store up to this snapshot epoch.
    pub ack_epoch: Option<u64>,
}

/// How to bring a pool buffer up to the canonical image.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum FillPlan {
    Full,
    Rects(Vec<Rect>),
}

/// The damage of the last `POOL` submitted frames, oldest first.
#[derive(Default, Debug)]
pub(crate) struct DamageHistory {
    frames: VecDeque<(FrameSeq, Vec<Rect>)>,
}

impl DamageHistory {
    fn push(&mut self, seq: FrameSeq, damage: Vec<Rect>) {
        if self.frames.len() == POOL {
            self.frames.pop_front();
        }
        self.frames.push_back((seq, damage));
    }

    fn clear(&mut self) {
        self.frames.clear();
    }

    /// The union of the damage of frames `after+1 ..= upto`, or `None` when
    /// the history does not reach back that far.
    fn since(&self, after: FrameSeq, upto: FrameSeq) -> Option<Vec<Rect>> {
        let mut rects = Vec::new();
        let mut want = after.0 + 1;
        for (seq, damage) in &self.frames {
            if seq.0 < want || seq.0 > upto.0 {
                continue;
            }
            if seq.0 != want {
                return None;
            }
            rects.extend_from_slice(damage);
            want += 1;
        }
        (want == upto.0 + 1).then_some(rects)
    }
}

/// One mapped top-level's frame state.
#[derive(Debug)]
pub(crate) struct WindowFrames {
    readiness: Readiness,
    /// A frame was delivered: the host showed the window.
    visible: bool,
    next_seq: u64,
    /// `backing_gen` = the backing's `DrawableId::as_u64` (never recycled).
    backing_gen: u64,
    /// The first seq of `backing_gen`.
    gen_start: FrameSeq,
    /// The newest delivered frame at `backing_gen`.
    last_delivered: Option<FrameSeq>,
    history: DamageHistory,
    /// Store snapshots awaiting `Delivered`: (frame, snapshot epoch).
    held: VecDeque<(FrameSeq, u64)>,
}

impl WindowFrames {
    pub(crate) fn new(backing_gen: u64) -> Self {
        Self {
            readiness: Readiness::Setup,
            visible: false,
            next_seq: 1,
            backing_gen,
            gen_start: FrameSeq(1),
            last_delivered: None,
            history: DamageHistory::default(),
            held: VecDeque::new(),
        }
    }

    pub(crate) fn readiness(&self) -> Readiness {
        self.readiness
    }

    pub(crate) fn backing_gen(&self) -> u64 {
        self.backing_gen
    }

    /// The newest delivered frame at this generation.
    pub(crate) fn last_delivered(&self) -> Option<FrameSeq> {
        self.last_delivered
    }

    pub(crate) fn on_setup_done(&mut self) {
        if self.readiness == Readiness::Setup {
            self.readiness = Readiness::ReadyFirst;
        }
    }

    /// `ReadyFirst` or `Ready`.
    pub(crate) fn can_submit(&self) -> bool {
        matches!(self.readiness, Readiness::ReadyFirst | Readiness::Ready)
    }

    /// A frame with `damage` (backing pixels, since the previous submitted
    /// frame) went to the host, read from the store snapshot at `epoch`.
    pub(crate) fn submitted(&mut self, damage: Vec<Rect>, epoch: Option<u64>) -> FrameSeq {
        let seq = FrameSeq(self.next_seq);
        self.next_seq += 1;
        self.history.push(seq, damage);
        if let Some(epoch) = epoch {
            self.held.push_back((seq, epoch));
        }
        self.readiness = Readiness::Busy;
        seq
    }

    /// No frame could go out (no swapchain image): wait for the host's
    /// `Ready` as after a frame.
    pub(crate) fn wait_for_ready(&mut self) {
        if self.can_submit() {
            self.readiness = Readiness::Busy;
        }
    }

    /// `Busy` → `Ready`, or back to `ReadyFirst` while nothing was shown
    /// (§4.5.1 liveness (a)).
    pub(crate) fn on_ready(&mut self) {
        if self.readiness == Readiness::Busy {
            self.readiness = if self.visible {
                Readiness::Ready
            } else {
                Readiness::ReadyFirst
            };
        }
    }

    pub(crate) fn on_delivered(&mut self, seq: FrameSeq) -> Delivery {
        if seq.0 >= self.next_seq {
            return Delivery {
                first: false,
                ack_epoch: None,
            };
        }
        // Only a frame of this generation shows the window (a pre-resize
        // frame delivered late does not).
        let current = seq >= self.gen_start;
        let first = current && !self.visible;
        if current {
            self.visible = true;
            if self.last_delivered.is_none_or(|d| d < seq) {
                self.last_delivered = Some(seq);
            }
        }
        let mut ack_epoch = None;
        while let Some(&(held, epoch)) = self.held.front() {
            if held > seq {
                break;
            }
            ack_epoch = Some(epoch);
            self.held.pop_front();
        }
        Delivery { first, ack_epoch }
    }

    /// Stays `Busy`: `Dropped` does not make the window ready (§4.5.1).
    pub(crate) fn on_dropped(&mut self, seq: FrameSeq) {
        self.held.retain(|&(held, _)| held != seq);
    }

    /// A new backing allocation: nothing is delivered at it, and its held
    /// snapshots (of the old pixmap) are never acked (§4.5).
    pub(crate) fn new_generation(&mut self, backing_gen: u64) {
        self.backing_gen = backing_gen;
        self.gen_start = FrameSeq(self.next_seq);
        self.last_delivered = None;
        self.history.clear();
        self.held.clear();
    }

    /// The host made the native window again: it shows nothing, takes no
    /// frame until its setup is done, and then a whole one, which no frame
    /// before it (delivered late, or a buffer it filled) stands for.
    pub(crate) fn restart(&mut self) {
        self.readiness = Readiness::Setup;
        self.visible = false;
        self.gen_start = FrameSeq(self.next_seq);
        self.last_delivered = None;
        self.history.clear();
        self.held.clear();
    }

    /// The damage frame `n` carries: since the last delivered frame, or
    /// `full` (§4.5.2).
    pub(crate) fn frame_damage(&self, n: FrameSeq, full: Rect) -> Vec<Rect> {
        self.last_delivered
            .and_then(|d| self.history.since(d, n))
            .unwrap_or_else(|| vec![full])
    }

    /// How to fill a buffer holding `buffer` = (seq, backing_gen) for frame `n`
    /// (§4.5.2); `None` is a fresh buffer.
    pub(crate) fn fill_plan(&self, buffer: Option<(FrameSeq, u64)>, n: FrameSeq) -> FillPlan {
        match buffer {
            Some((seq, backing_gen))
                if backing_gen == self.backing_gen && seq >= self.gen_start =>
            {
                self.history
                    .since(seq, n)
                    .map_or(FillPlan::Full, FillPlan::Rects)
            }
            _ => FillPlan::Full,
        }
    }
}
