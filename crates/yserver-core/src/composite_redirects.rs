//! COMPOSITE redirect bookkeeping, ported from Xorg's
//! `composite/compalloc.c`.
//!
//! Xorg keeps two per-window lists of per-client records
//! (`CompClientWindowRec`): `cw->clients`, the redirects that apply to the
//! window itself, and `csw->clients`, the `RedirectSubwindows` records that
//! apply to its children. A subwindows redirect is not inherited at lookup
//! time: it adds one record per child to that child's own list
//! (`compRedirectSubwindows`, and `compCreateWindow` /
//! `compReparentWindow` for later children), owned by the same client with
//! the same update mode. So `UnredirectWindow(child)` under a
//! `RedirectSubwindows(parent)` frees that record and the child is no
//! longer redirected; a later `RedirectWindow(child, Manual)` then succeeds.
//!
//! This type is the bookkeeping only. Callers compare
//! [`CompositeRedirects::window_mode`] before and after a change to drive
//! the window's backing (Xorg `compCheckRedirect`).

use std::collections::HashMap;

use yserver_protocol::x11::{ClientId, ResourceId};

use crate::server::{CompositeRedirectMode, RedirectRecord};

/// A redirect that Xorg refuses with `BadAccess`: a second Manual
/// redirect, whichever client asks (`compalloc.c:155-158`, `:336-339`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualTaken;

#[derive(Debug, Default)]
pub struct CompositeRedirects {
    /// `cw->clients`, newest first.
    windows: HashMap<ResourceId, Vec<RedirectRecord>>,
    /// `csw->clients`, newest first.
    subwindows: HashMap<ResourceId, Vec<RedirectRecord>>,
}

fn update_of(records: &[RedirectRecord]) -> Option<CompositeRedirectMode> {
    if records.is_empty() {
        None
    } else if records
        .iter()
        .any(|r| r.mode == CompositeRedirectMode::Manual)
    {
        Some(CompositeRedirectMode::Manual)
    } else {
        Some(CompositeRedirectMode::Automatic)
    }
}

fn remove_one(
    map: &mut HashMap<ResourceId, Vec<RedirectRecord>>,
    window: ResourceId,
    owner: ClientId,
    mode: CompositeRedirectMode,
) -> bool {
    let Some(list) = map.get_mut(&window) else {
        return false;
    };
    let Some(at) = list.iter().position(|r| r.owner == owner && r.mode == mode) else {
        return false;
    };
    list.remove(at);
    if list.is_empty() {
        map.remove(&window);
    }
    true
}

impl CompositeRedirects {
    /// The window's effective update mode (`cw->update`), `None` when it
    /// is not redirected (`cw == NULL`): Manual while any record is Manual.
    #[must_use]
    pub fn window_mode(&self, window: ResourceId) -> Option<CompositeRedirectMode> {
        self.windows.get(&window).and_then(|l| update_of(l))
    }

    /// The `RedirectSubwindows` mode on `parent` (`csw->update`).
    #[must_use]
    pub fn subwindows_mode(&self, parent: ResourceId) -> Option<CompositeRedirectMode> {
        self.subwindows.get(&parent).and_then(|l| update_of(l))
    }

    #[must_use]
    pub fn window_records(&self, window: ResourceId) -> &[RedirectRecord] {
        self.windows.get(&window).map_or(&[], Vec::as_slice)
    }

    #[must_use]
    pub fn subwindows_records(&self, parent: ResourceId) -> &[RedirectRecord] {
        self.subwindows.get(&parent).map_or(&[], Vec::as_slice)
    }

    /// The parents whose subwindows list holds a record of `owner`.
    #[must_use]
    pub fn subwindows_parents_of(&self, owner: ClientId) -> Vec<ResourceId> {
        let mut parents: Vec<ResourceId> = self
            .subwindows
            .iter()
            .filter(|(_, list)| list.iter().any(|r| r.owner == owner))
            .map(|(parent, _)| *parent)
            .collect();
        parents.sort_unstable_by_key(|p| p.0);
        parents
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.subwindows.is_empty()
    }

    /// Xorg `compRedirectWindow` (`compalloc.c:133-230`), minus the
    /// overlay and root checks the caller makes.
    ///
    /// # Errors
    ///
    /// [`ManualTaken`] for a Manual redirect on a window that already has
    /// one, from any client.
    pub fn redirect_window(
        &mut self,
        window: ResourceId,
        record: RedirectRecord,
    ) -> Result<(), ManualTaken> {
        let list = self.windows.entry(window).or_default();
        if record.mode == CompositeRedirectMode::Manual
            && list.iter().any(|r| r.mode == CompositeRedirectMode::Manual)
        {
            if list.is_empty() {
                self.windows.remove(&window);
            }
            return Err(ManualTaken);
        }
        list.insert(0, record);
        Ok(())
    }

    /// Xorg `compUnredirectWindow` (`compalloc.c:313-326`): frees one of
    /// `owner`'s records with that mode, whether `RedirectWindow` or a
    /// subwindows redirect created it. `false` is Xorg's `BadValue`.
    pub fn unredirect_window(
        &mut self,
        window: ResourceId,
        owner: ClientId,
        mode: CompositeRedirectMode,
    ) -> bool {
        remove_one(&mut self.windows, window, owner, mode)
    }

    /// Xorg `compRedirectSubwindows` (`compalloc.c:328-399`): a record on
    /// `parent`'s subwindows list plus one on each child's own list,
    /// `children` top first (`lastChild` → `prevSib`). A child that refuses
    /// rolls back the ones already done.
    ///
    /// # Errors
    ///
    /// [`ManualTaken`] for a second Manual subwindows redirect on `parent`,
    /// or a Manual one when a child already has a Manual redirect.
    pub fn redirect_subwindows(
        &mut self,
        parent: ResourceId,
        children: &[ResourceId],
        record: RedirectRecord,
    ) -> Result<(), ManualTaken> {
        if record.mode == CompositeRedirectMode::Manual
            && self
                .subwindows_records(parent)
                .iter()
                .any(|r| r.mode == CompositeRedirectMode::Manual)
        {
            return Err(ManualTaken);
        }
        for (done, child) in children.iter().enumerate() {
            if let Err(err) = self.redirect_window(*child, record) {
                for undo in &children[..done] {
                    self.unredirect_window(*undo, record.owner, record.mode);
                }
                return Err(err);
            }
        }
        self.subwindows.entry(parent).or_default().insert(0, record);
        Ok(())
    }

    /// Xorg `compUnredirectSubwindows` → `compFreeClientSubwindows`
    /// (`compalloc.c:405-475`): drops `owner`'s subwindows record and frees
    /// one of `owner`'s same-mode records on every child — including one a
    /// later `RedirectWindow(child)` made. `false` is Xorg's `BadValue`.
    pub fn unredirect_subwindows(
        &mut self,
        parent: ResourceId,
        children: &[ResourceId],
        owner: ClientId,
        mode: CompositeRedirectMode,
    ) -> bool {
        if !remove_one(&mut self.subwindows, parent, owner, mode) {
            return false;
        }
        for child in children {
            self.unredirect_window(*child, owner, mode);
        }
        true
    }

    /// A child created under `parent` gets a record per subwindows record
    /// of `parent`, refusals ignored (Xorg `compCreateWindow`,
    /// `compwindow.c:579-589`).
    pub fn redirect_new_subwindow(&mut self, parent: ResourceId, child: ResourceId) {
        for record in self.subwindows_records(parent).to_vec() {
            let _ = self.redirect_window(child, record);
        }
    }

    /// Reparent: `compUnredirectOneSubwindow(old)` then
    /// `compRedirectOneSubwindow(new)` (`compwindow.c:453-454`,
    /// `compalloc.c:500-529`); each stops at its first failure.
    pub fn reparent_subwindow(
        &mut self,
        old_parent: ResourceId,
        new_parent: ResourceId,
        child: ResourceId,
    ) {
        for record in self.subwindows_records(old_parent).to_vec() {
            if !self.unredirect_window(child, record.owner, record.mode) {
                break;
            }
        }
        for record in self.subwindows_records(new_parent).to_vec() {
            if self.redirect_window(child, record).is_err() {
                break;
            }
        }
    }

    /// Drop every record on the destroyed `windows` (Xorg
    /// `compDestroyWindow`).
    pub fn forget_windows(&mut self, windows: &[ResourceId]) {
        for window in windows {
            self.windows.remove(window);
            self.subwindows.remove(window);
        }
    }

    /// Free every record `owner` holds, as its resources go with it.
    /// Returns each window whose own list changed with its mode before, so
    /// its backing can be re-checked.
    pub fn forget_client(
        &mut self,
        owner: ClientId,
    ) -> Vec<(ResourceId, Option<CompositeRedirectMode>)> {
        let mut changed = Vec::new();
        self.windows.retain(|window, list| {
            let before = update_of(list.as_slice());
            let len = list.len();
            list.retain(|r| r.owner != owner);
            if list.len() != len {
                changed.push((*window, before));
            }
            !list.is_empty()
        });
        self.subwindows.retain(|_, list| {
            list.retain(|r| r.owner != owner);
            !list.is_empty()
        });
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: ResourceId = ResourceId(0x0000_0001);
    const W: ResourceId = ResourceId(0x0020_0001);
    const V: ResourceId = ResourceId(0x0020_0002);
    const FRAME: ResourceId = ResourceId(0x0020_0003);
    const WM: ClientId = ClientId(1);
    const OTHER: ClientId = ClientId(2);
    const MANUAL: CompositeRedirectMode = CompositeRedirectMode::Manual;
    const AUTOMATIC: CompositeRedirectMode = CompositeRedirectMode::Automatic;

    fn rec(owner: ClientId, mode: CompositeRedirectMode) -> RedirectRecord {
        RedirectRecord { mode, owner }
    }

    // Measured on Xvfb 21.1: the compositor's fullscreen round.
    #[test]
    fn child_unredirected_under_subwindows_redirect_can_be_redirected_again() {
        let mut r = CompositeRedirects::default();
        r.redirect_subwindows(ROOT, &[W], rec(WM, MANUAL)).unwrap();
        assert_eq!(r.window_mode(W), Some(MANUAL));
        assert_eq!(
            r.redirect_window(W, rec(WM, MANUAL)),
            Err(ManualTaken),
            "muffin's probe: a second Manual redirect, same client"
        );
        assert!(r.unredirect_window(W, WM, MANUAL));
        assert_eq!(r.window_mode(W), None);
        assert!(
            !r.unredirect_window(W, WM, MANUAL),
            "BadValue the second time"
        );
        assert_eq!(r.redirect_window(W, rec(WM, MANUAL)), Ok(()));
        assert_eq!(r.window_mode(W), Some(MANUAL));
        assert_eq!(r.subwindows_mode(ROOT), Some(MANUAL));
    }

    #[test]
    fn manual_is_exclusive_automatic_is_shared() {
        let mut r = CompositeRedirects::default();
        r.redirect_window(W, rec(WM, MANUAL)).unwrap();
        assert_eq!(r.redirect_window(W, rec(OTHER, MANUAL)), Err(ManualTaken));
        assert_eq!(r.redirect_window(W, rec(OTHER, AUTOMATIC)), Ok(()));
        assert_eq!(r.window_mode(W), Some(MANUAL));
        assert!(r.unredirect_window(W, WM, MANUAL));
        assert_eq!(
            r.window_mode(W),
            Some(AUTOMATIC),
            "compFreeClientWindow falls back to Automatic"
        );
        assert!(
            !r.unredirect_window(W, WM, AUTOMATIC),
            "owner and mode must match"
        );
        assert!(r.unredirect_window(W, OTHER, AUTOMATIC));
        assert!(r.is_empty());
    }

    #[test]
    fn second_manual_subwindows_redirect_is_refused() {
        let mut r = CompositeRedirects::default();
        r.redirect_subwindows(ROOT, &[W], rec(WM, MANUAL)).unwrap();
        assert_eq!(
            r.redirect_subwindows(ROOT, &[W], rec(OTHER, MANUAL)),
            Err(ManualTaken)
        );
        assert_eq!(r.window_records(W), &[rec(WM, MANUAL)]);
    }

    #[test]
    fn subwindows_redirect_refused_by_a_child_rolls_back() {
        let mut r = CompositeRedirects::default();
        r.redirect_window(V, rec(OTHER, MANUAL)).unwrap();
        assert_eq!(
            r.redirect_subwindows(ROOT, &[W, V], rec(WM, MANUAL)),
            Err(ManualTaken)
        );
        assert_eq!(r.window_mode(W), None);
        assert_eq!(r.subwindows_mode(ROOT), None);
        assert_eq!(r.window_records(V), &[rec(OTHER, MANUAL)]);
    }

    #[test]
    fn unredirect_subwindows_frees_a_later_redirect_window_of_that_client() {
        // compFreeClientSubwindows calls compUnredirectWindow on every child,
        // which frees whichever same-client, same-mode record it finds.
        let mut r = CompositeRedirects::default();
        r.redirect_subwindows(ROOT, &[W, V], rec(WM, MANUAL))
            .unwrap();
        assert!(r.unredirect_window(W, WM, MANUAL));
        r.redirect_window(W, rec(WM, MANUAL)).unwrap();
        r.redirect_window(V, rec(OTHER, AUTOMATIC)).unwrap();
        assert!(r.unredirect_subwindows(ROOT, &[W, V], WM, MANUAL));
        assert_eq!(r.window_mode(W), None);
        assert_eq!(r.window_mode(V), Some(AUTOMATIC));
        assert!(!r.unredirect_subwindows(ROOT, &[W, V], WM, MANUAL));
    }

    #[test]
    fn new_and_reparented_children_follow_the_subwindows_records() {
        let mut r = CompositeRedirects::default();
        r.redirect_subwindows(ROOT, &[], rec(WM, MANUAL)).unwrap();
        r.redirect_new_subwindow(ROOT, W);
        assert_eq!(r.window_mode(W), Some(MANUAL));
        // Into a frame nobody redirects: the root's record goes.
        r.reparent_subwindow(ROOT, FRAME, W);
        assert_eq!(r.window_mode(W), None);
        r.reparent_subwindow(FRAME, ROOT, W);
        assert_eq!(r.window_mode(W), Some(MANUAL));
        // An own RedirectWindow by another client survives the round.
        r.redirect_window(W, rec(OTHER, AUTOMATIC)).unwrap();
        r.reparent_subwindow(ROOT, FRAME, W);
        assert_eq!(r.window_records(W), &[rec(OTHER, AUTOMATIC)]);
    }

    #[test]
    fn forget_client_reports_the_windows_it_changed() {
        let mut r = CompositeRedirects::default();
        r.redirect_subwindows(ROOT, &[W], rec(WM, MANUAL)).unwrap();
        r.redirect_window(V, rec(OTHER, AUTOMATIC)).unwrap();
        assert_eq!(r.forget_client(WM), vec![(W, Some(MANUAL))]);
        assert_eq!(r.subwindows_mode(ROOT), None);
        assert_eq!(r.window_mode(V), Some(AUTOMATIC));
    }
}
