use super::*;

/// Allocate the redirect's reason-1 backing for `window` and set
/// `Window.redirected_backing`. Shared between B.6a (single-window
/// REDIRECT_WINDOW), B.6b (REDIRECT_SUBWINDOWS walking children),
/// and the CreateWindow child-of-redirected hook.
///
/// Silently no-ops if the window doesn't exist, has no host XID
/// yet, or the backend rejects the allocation. The caller doesn't
/// emit a protocol error in any of those cases — the redirect
/// record is already in place, so a later paint via
/// `host_drawable_target` will simply fall back to the window's
/// own host XID until a future event populates the backing.
///
/// Stage 4b: wired into the COMPOSITE `RedirectWindow` /
/// `RedirectSubwindows` dispatch, gated on
/// `Backend::supports_redirect_activation()`. v1 (`KmsBackend`)
/// returns `false`, preserving the post-`3751c11` revert that
/// fixed MATE; v2 (`KmsBackend`) overrides to `true` and the
/// full allocate + participation-flip path runs.
///
/// `mode` drives the scene-participation flip after a successful
/// allocate: Manual → window participating=false (the external
/// compositor drives presentation), Automatic → window
/// participating=true AND backing participating=true (paint
/// resolves through the backing and the scene walk picks it up
/// via W's `redirected_target` indirection in 4c).
pub(super) fn activate_redirect_backing_for(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
    mode: crate::server::CompositeRedirectMode,
) {
    // Xorg compCheckRedirect (composite/compwindow.c:156-170): the
    // overlay window is NEVER actually redirected — should=FALSE for
    // pWin == cs->pOverlayWin — regardless of trigger
    // (RedirectSubwindows(root), explicit RedirectWindow, or
    // auto-redirect-on-map). The COW is a real child of root (Phase 2),
    // so RedirectSubwindows(root, Manual) would otherwise hand it a
    // Manual backing + scene_participating=false, and the Phase 3
    // Manual-skip would drop the whole composited desktop from scanout.
    // The COW reaches scanout via the normal paint path.
    if window == COMPOSITE_OVERLAY_WINDOW {
        return;
    }
    // compCheckRedirect allocates only for a realized window (compwindow.c:162); realize does it later.
    if state
        .resources
        .window(window)
        .is_none_or(|w| w.map_state != MapState::Viewable)
    {
        return;
    }
    // Mode-flip on an existing redirect is routed through
    // `flip_redirect_target_mode` upstream — don't reallocate
    // here (Xorg preserves the backing per
    // `xserver/composite/compwindow.c:172` +
    // `compositeproto.txt:80`). If the caller reaches us with a
    // populated `redirected_backing` it's a same-mode idempotent
    // call (or a stale handoff — log + skip rather than crash).
    if state
        .resources
        .window(window)
        .is_some_and(|w| w.redirected_backing.is_some())
    {
        log::debug!(
            "activate_redirect_backing_for(0x{:x}): backing already present (same-mode \
             idempotent); skipping reallocation",
            window.0
        );
        return;
    }
    let snapshot = state
        .resources
        .window(window)
        .map(|w| (w.host_xid, w.width, w.height, w.border_width, w.depth));
    let Some((Some(host_window), w_width, w_height, w_border, w_depth)) = snapshot else {
        return;
    };
    // #133 step 3 (3.3) — a redirect backing is allocated at the
    // BORDERED extent, `(w + 2bw) x (h + 2bw)`, and holds the window at
    // its OUTER origin with the content `bw` inside: Xorg
    // `compAllocPixmap` computes exactly `w = width + (bw << 1)` /
    // `h = height + (bw << 1)` and hands that to `compNewPixmap`
    // (`composite/compalloc.c:608-618`). The render backend's seed /
    // inferior-reconstruct paths place content at `(bw, bw)` in this
    // pixmap, and `NameWindowPixmap` hands the whole bordered image to
    // the compositor, so the border is part of it by design.
    // `bordered_backing_extent` is the identity at `bw == 0`.
    let (w_width, w_height) = bordered_backing_extent(w_width, w_height, w_border);
    match backend.allocate_redirected_backing(origin, host_window, w_width, w_height, w_depth) {
        Ok(host_pixmap) => {
            if let Some(w) = state.resources.window_mut(window) {
                w.redirected_backing = Some(crate::resources::RedirectedBacking {
                    host_pixmap,
                    width: w_width,
                    height: w_height,
                    depth: w_depth,
                });
            }
            // YSERVER_TRAY_DEBUG: capture-side timeline for the systray
            // sliver/blank race — when each (socket) window's redirect
            // backing is allocated, its size, and the mode. Correlate
            // against the `TRAY fill/damage` lines (which carry the plug
            // child geometry at Clear time). REMOVE with the rest of the
            // tray diag once the redirect-backing race is fixed.
            if std::env::var_os("YSERVER_TRAY_DEBUG").is_some() {
                log::info!(
                    target: "yserver_core::core_loop::tray",
                    "TRAY redirect-alloc win=0x{:x} backing={w_width}x{w_height}d{w_depth} mode={mode:?}",
                    window.0,
                );
            }
            // Scene-participation flip. Spec §285+360 names
            // redirect-state change as a scene-structure damage
            // source; the v2 impl of these setters fires that
            // damage internally so the protocol handler just
            // makes the calls.
            //
            // W: Automatic → true (scene walks W, samples B via
            // 4c.3 indirection). Manual → false (the external
            // compositor owns presentation and reintroduces the
            // window via its own output/COW surface, while the
            // post-6ffd370 scene still emits W's own backing as
            // its source — both controlled by the B flag below).
            //
            // B: always true. Post-6ffd370 the scene samples B
            // through W's `redirected_target` in both modes
            // (Automatic via W's own entry, Manual via the parent
            // backing emit), so `store.damage(B_id, ..)` from
            // every paint into the backing must accumulate.
            // Leaving B non-participating in Manual mode silently
            // drops all those damages and buffer-age compose
            // retains stale BO pixels except where cursor damage
            // happens to overlap.
            let window_participating =
                matches!(mode, crate::server::CompositeRedirectMode::Automatic);
            if let Err(err) =
                backend.set_window_scene_participation(origin, host_window, window_participating)
            {
                log::warn!(
                    "set_window_scene_participation(0x{:x}, {window_participating}) failed: {err}",
                    window.0
                );
            }
            if let Err(err) = backend.set_backing_scene_participation(origin, host_pixmap, true) {
                log::warn!(
                    "set_backing_scene_participation(0x{:x}, true) failed: {err}",
                    host_pixmap.as_raw()
                );
            }
            // Redirecting an already-viewable window hands the compositor a
            // valid NameWindowPixmap source immediately, but the backing seed
            // above happened before any later client paint. If we don't emit an
            // initial full-window damage wakeup here, compositors such as picom
            // can leave the freshly-redirected window absent from the COW until
            // some unrelated later event (click, move, resize) dirties it.
            //
            // This mirrors the "first viewable contents need one wakeup" rule
            // on MapWindow: once the redirected backing exists and participation
            // flips are installed, wake DAMAGE subscribers so they pull the
            // seeded pixels into their own composite output immediately.
            if state
                .resources
                .window(window)
                .is_some_and(|w| w.map_state == MapState::Viewable)
            {
                let _dropped = accumulate_damage_full_to_state(state, window);
                // #143 — and the RING, which `accumulate_damage_full_to_state`
                // cannot express: its rect is `(0, 0, width, height)`, i.e.
                // `winSize`, which excludes the border by construction. The
                // backing we just allocated is the BORDERED extent and
                // `allocate_redirected_backing` paints the ring into it, so
                // the compositor has to be told about those pixels too.
                // Xorg gets this for free — `compSetPixmapVisitWindow` queues
                // `compRepaintBorder` whenever `bw != 0`
                // (`composite/compwindow.c:137-139`), and that repaint is an
                // ordinary GC op the DAMAGE wrapper sees. Identity at
                // `bw == 0`.
                let _dropped = accumulate_damage_border_to_state(state, window);
            }
        }
        Err(err) => {
            log::warn!(
                "activate_redirect_backing_for(0x{:x}): allocate failed: {err}",
                window.0
            );
        }
    }
}

/// Stage 4b.8: same-owner mode-flip handler. When a client
/// re-issues `Redirect{Window,Subwindows}(W, new_mode)` while it
/// already owns a record at the same key with a different mode,
/// Xorg's `compCheckRedirect`
/// (`xserver/composite/compwindow.c:172`) preserves the backing
/// pixmap and every `NameWindowPixmap` alias; only `redirectDraw`
/// flips. Composite spec line 80: "old named pixmaps remain
/// allocated until FreePixmap."
///
/// In v2 the equivalent is: keep `Window.redirected_backing` +
/// `KmsCore.alias_registry` + `KmsCore.host_window_to_backing` +
/// `Drawable.redirected_target` untouched; only fire the
/// participation-flip pair for the new mode.
///
/// **No re-seed** per the plan's codex-round-6 decision: the
/// backing's current content is whatever the compositor has been
/// reading from (Automatic) or the still-frozen pre-redirect snapshot
/// (Manual just-flipped-from-Automatic). Running `copy_area(W, B)`
/// would replace that with W's storage which under Manual is empty —
/// strictly worse than the existing backing contents.
fn flip_redirect_target_mode(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    target: ResourceId,
    new_mode: crate::server::CompositeRedirectMode,
) {
    let snapshot = state.resources.window(target).map(|w| {
        (
            w.host_xid,
            w.redirected_backing.as_ref().map(|b| b.host_pixmap),
        )
    });
    let Some((Some(host_window), Some(backing))) = snapshot else {
        // No existing backing → fall back to a fresh activation.
        // This shouldn't normally fire (the caller gates on
        // `prev.mode != new_mode` which implies a prior record
        // existed, and any prior record under
        // `supports_redirect_activation()` allocated a backing),
        // but stay defensive.
        log::debug!(
            "flip_redirect_target_mode(0x{:x}): no existing backing; falling back to allocate",
            target.0
        );
        activate_redirect_backing_for(state, backend, origin, target, new_mode);
        return;
    };
    // See activate_redirect_backing_for for the rationale: B is
    // always scene-participating because the post-6ffd370 scene
    // samples B via `redirected_target` in both modes; only W's
    // own scene presence toggles with mode.
    let window_participating = matches!(new_mode, crate::server::CompositeRedirectMode::Automatic);
    if let Err(err) =
        backend.set_window_scene_participation(origin, host_window, window_participating)
    {
        log::warn!(
            "flip_redirect_target_mode(0x{:x}, {window_participating}): \
             set_window_scene_participation failed: {err}",
            target.0
        );
    }
    if let Err(err) = backend.set_backing_scene_participation(origin, backing, true) {
        log::warn!(
            "flip_redirect_target_mode(0x{:x}, true): \
             set_backing_scene_participation failed: {err}",
            target.0
        );
    }
}

/// Xorg `compCheckRedirect` after `window`'s redirect records changed from
/// effective mode `before`: allocate, flip or drop its backing to match.
/// Allocation and the flip need `supports_redirect_activation()`; a
/// teardown is a no-op for a window that never got a backing.
pub(crate) fn sync_redirect_backing(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
    before: Option<crate::server::CompositeRedirectMode>,
) {
    let after = state.composite_redirects.window_mode(window);
    if before == after {
        return;
    }
    match (before, after) {
        (_, None) => crate::core_loop::process_disconnect::teardown_redirect_for_window(
            state, backend, origin, window,
        ),
        _ if !backend.supports_redirect_activation() => {}
        (None, Some(mode)) => activate_redirect_backing_for(state, backend, origin, window, mode),
        (Some(_), Some(mode)) => flip_redirect_target_mode(state, backend, origin, window, mode),
    }
}

/// Point GLX pixmap `glx_xid` at `new_host`, moving its export-lifetime ref along (acquire NEW, release OLD).
pub(super) fn retarget_glx_pixmap_export(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    glx_xid: u32,
    new_host: u32,
) {
    let Some(drawable) = state.glx_drawables.get_mut(&glx_xid) else {
        return;
    };
    let old_host = drawable.glx_export_host_xid.replace(new_host);
    if old_host == Some(new_host) {
        return;
    }
    backend.acquire_glx_pixmap_export(new_host);
    if let Some(old_host) = old_host {
        backend.release_glx_pixmap_export(old_host);
    }
}

/// A window that just became viewable under an existing redirect (its own
/// `RedirectWindow`, or its parent's `RedirectSubwindows`) gets a fresh
/// backing, as Xorg's `compRealizeWindow` → `compCheckRedirect` →
/// `compAllocPixmap` does (`composite/compwindow.c:274`, `:173-174`).
///
/// Must be called AFTER `backend.map_subwindow`: `map_subwindow` blindly
/// sets `scene_participating = true`, and the Manual participation flip
/// inside `activate_redirect_backing_for` must land last. Callers walk the
/// delta's `became_viewable` parent first, so the seed finds the parent's
/// storage (or backing) already in place.
fn realize_redirect_backing(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
) {
    if !backend.supports_redirect_activation() {
        return;
    }
    if state
        .resources
        .window(window)
        .is_none_or(|w| w.redirected_backing.is_some())
    {
        return;
    }
    let Some(mode) = effective_redirect_mode_for_window(state, window) else {
        return;
    };
    activate_redirect_backing_for(state, backend, origin, window, mode);
}

/// Apply a viewability delta to COMPOSITE backings: the windows that became
/// unviewable (child first) drop theirs, those that became viewable (parent
/// first) get a fresh one. The redirect records are untouched.
pub(super) fn apply_viewability_delta_to_redirects(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    delta: &ViewabilityDelta,
) {
    for window in &delta.became_unviewable {
        let _ = crate::core_loop::process_disconnect::unrealize_redirect_backing(
            state, backend, origin, *window,
        );
    }
    for window in &delta.became_viewable {
        realize_redirect_backing(state, backend, origin, *window);
    }
}

/// Host xid of a window whose storage follows its viewability; the root and the COW own theirs.
pub(super) fn storage_lifecycle_host_xid(state: &ServerState, window: ResourceId) -> Option<u32> {
    if window == ROOT_WINDOW || window == COMPOSITE_OVERLAY_WINDOW {
        return None;
    }
    state
        .resources
        .window(window)
        .and_then(|w| w.host_xid)
        .map(|h| h.as_raw())
}

/// Windows that became viewable get storage, parent first; call before their redirect backings.
pub(super) fn realize_storage_for_delta(
    state: &ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    delta: &ViewabilityDelta,
) {
    for window in &delta.became_viewable {
        if let Some(xid) = storage_lifecycle_host_xid(state, *window)
            && let Err(err) = backend.realize_window_storage(origin, xid)
        {
            log::warn!("realize_window_storage(0x{xid:x}) failed: {err}");
        }
    }
}

/// Windows that became unviewable drop storage, child first; call after their redirect backings.
pub(super) fn release_storage_for_delta(
    state: &ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    delta: &ViewabilityDelta,
) {
    for window in &delta.became_unviewable {
        if let Some(xid) = storage_lifecycle_host_xid(state, *window)
            && let Err(err) = backend.release_window_storage(origin, xid)
        {
            log::warn!("release_window_storage(0x{xid:x}) failed: {err}");
        }
    }
}

/// Re-apply a window's effective COMPOSITE redirect mode after a
/// map operation. `map_subwindow` flips storage visible by default;
/// Manual-redirected windows must be forced back to
/// `scene_participating=false`, while Automatic windows stay on-scene.
pub(super) fn reapply_redirect_mode_after_map(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
) {
    let Some(mode) = effective_redirect_mode_for_window(state, window) else {
        return;
    };
    let Some(host_xid) = state.resources.window(window).and_then(|w| w.host_xid) else {
        return;
    };
    let participating = matches!(mode, crate::server::CompositeRedirectMode::Automatic);
    if let Err(err) = backend.set_window_scene_participation(origin, host_xid, participating) {
        log::warn!(
            "reapply_redirect_mode_after_map(0x{:x}): \
             set_window_scene_participation({participating}) failed: {err}",
            window.0
        );
    }
}

/// Resize-time bookkeeping for COMPOSITE-redirected windows. yserver
/// rotates the redirected backing to fresh storage sized to the new
/// geometry. Existing `NameWindowPixmap` aliases are also retargeted
/// onto the new backing as a compatibility measure for picom/openbox:
/// in hardware repros picom kept sampling the pre-resize backing for
/// many seconds after ConfigureNotify, leaving white right/bottom
/// strips and frozen content. Following Xorg compatibility here is
/// more important than the earlier frozen-alias theory.
///
/// On pure moves, `force_reallocate=true` routes through the same
/// rotate path even when the size is unchanged. That hands the
/// compositor a fresh backing object at the window's new position
/// instead of reusing the original named pixmap indefinitely.
///
/// Sequence when the window is redirected (release-then-allocate
/// order is load-bearing — see below):
///   1. Snapshot the existing backing handle.
///   2. Release the old backing's reason-1 hold via
///      `release_redirected_backing`. If `composite_named_pixmaps`
///      aliases still reference it, refcount stays > 0 and the
///      backing survives — its content is frozen at pre-resize.
///      If no aliases hold it, the backing is freed. This also
///      clears the backend's `host_window_to_backing[W]` slot so the
///      next allocate doesn't short-circuit on the now-stale entry.
///   3. Allocate a new backing at the new (width, height, depth).
///      With the slot cleared in step 2, the backend's idempotent
///      lookup misses and we actually get fresh storage sized to
///      `new_width × new_height`.
///   4. Repoint `Window.redirected_backing` at the new backing.
///
/// Release-then-allocate order is mandatory because the backend's
/// `allocate_redirected_backing` is idempotent on
/// `host_window_to_backing[W]`: an allocate-then-release pass would
/// return the EXISTING backing handle (ignoring `new_width` /
/// `new_height`), and the subsequent release would then destroy the
/// very pixmap we just decided to keep. Hardware smoke caught this
/// when marco resized a 25-tall mate-panel to 28 tall: the next
/// `NameWindowPixmap` returned `NotFound` because the backing had
/// been freed under it.
///
/// `composite_named_pixmaps` is updated to point at the new backing
/// and new geometry. Each alias drops one hold on OLD and takes one
/// hold on NEW in the backend alias registry so the lifetime model
/// stays balanced.
///
/// When the window is **not** redirected (no backing), this is a
/// no-op — `composite_named_pixmaps` should be empty by
/// construction (the protocol layer only creates aliases on
/// redirected windows).
/// #133 step 3 (3.3) — a COMPOSITE redirect backing's extent:
/// `(w + 2bw) x (h + 2bw)`, mirroring Xorg `compAllocPixmap`
/// (`composite/compalloc.c:608-618`, `w = width + (bw << 1)`). The
/// window sits at its OUTER origin inside it with the client content
/// `bw` in, so the border is part of the named pixmap by design.
/// Identity at `bw == 0`.
fn bordered_backing_extent(width: u16, height: u16, border_width: u16) -> (u16, u16) {
    let bw2 = border_width.saturating_mul(2);
    (width.saturating_add(bw2), height.saturating_add(bw2))
}

pub(super) fn rotate_redirected_backing_on_resize(
    state: &mut ServerState,
    backend: &mut dyn Backend,
    origin: Option<OriginContext>,
    window: ResourceId,
    new_width: u16,
    new_height: u16,
    force_reallocate: bool,
    old_border_width: u16,
) {
    let snapshot = state.resources.window(window).and_then(|w| {
        w.redirected_backing.as_ref().map(|b| {
            (
                b.host_pixmap,
                b.width,
                b.height,
                w.host_xid,
                w.depth,
                w.border_width,
            )
        })
    });
    let Some((old_backing, old_width, old_height, host_window, depth, border_width)) = snapshot
    else {
        return;
    };
    let Some(host_window) = host_window else {
        return;
    };
    // #133 step 3 (3.3): the caller's `new_width`/`new_height` are the
    // window's CONTENT size; the backing is the bordered extent (see
    // `activate_redirect_backing_for`). Identity at `bw == 0`.
    let (new_width, new_height) = bordered_backing_extent(new_width, new_height, border_width);

    if !force_reallocate
        && old_border_width == border_width
        && backend.redirected_backing_can_fit(old_backing, new_width, new_height, depth)
    {
        if let Some(w) = state.resources.window_mut(window)
            && let Some(backing) = &mut w.redirected_backing
        {
            backing.width = new_width;
            backing.height = new_height;
            backing.depth = depth;
        }
        let _ = backend.update_redirected_backing_geometry(
            origin,
            old_backing,
            new_width,
            new_height,
            depth,
        );
        let aliases_to_retarget = state
            .resources
            .window(window)
            .map(|w| w.composite_named_pixmaps.clone())
            .unwrap_or_default();
        for alias in &aliases_to_retarget {
            let _ = state.resources.update_pixmap_geometry(
                alias.client_pixmap,
                new_width,
                new_height,
                depth,
            );
        }
        if let Some(w) = state.resources.window_mut(window) {
            for alias in &mut w.composite_named_pixmaps {
                alias.width = new_width;
                alias.height = new_height;
            }
        }
        return;
    }

    // Take a rotate-scoped retain on OLD's storage BEFORE the
    // release. Without it, the no-alias case (no NameWindowPixmap
    // outstanding on this backing) sees release_redirected_backing's
    // alias_registry.decref hit refcount=0 → free_pixmap → store
    // entry dropped → the copy_area below reads from a freed handle
    // (observed as `copy_area dropped — src unknown` in HW smoke
    // 2026-05-20 17:28:10Z). Paired with `drop_backing_storage`
    // after the copy.
    if let Err(err) = backend.retain_backing_storage(origin, old_backing) {
        log::warn!(
            "rotate_redirected_backing_on_resize: retain_backing_storage(0x{:x}) failed: {err}",
            old_backing.as_raw()
        );
    }

    // Release the old backing FIRST so the backend's
    // `host_window_to_backing[W]` slot is clear before the allocate
    // below — otherwise allocate's idempotent short-circuit would
    // hand back the about-to-be-released handle and we'd free the
    // very pixmap we just chose to keep. Composite spec lets the
    // backing survive (alias-frozen) if `NameWindowPixmap` references
    // still hold it via the alias registry; the rotate-retain above
    // covers the no-alias case.
    if let Err(err) = backend.release_redirected_backing(origin, old_backing) {
        log::warn!(
            "rotate_redirected_backing_on_resize: release_redirected_backing(0x{:x}) failed: {err}",
            old_backing.as_raw()
        );
        // Don't bail — try the allocate anyway. Worst case the
        // allocate also fails and we just leave the window unrouted;
        // better than leaving the OLD backing pointing somewhere
        // stale.
    }

    // Now allocate fresh at the new size. With the slot cleared by
    // the release above, the idempotent short-circuit misses and we
    // get a backing actually sized to `new_width × new_height`.
    let new_backing = match backend.allocate_redirected_backing(
        origin,
        host_window,
        new_width,
        new_height,
        depth,
    ) {
        Ok(h) => h,
        Err(err) => {
            log::warn!(
                "rotate_redirected_backing_on_resize(0x{:x}, {new_width}x{new_height}): \
                 allocate failed: {err}",
                window.0
            );
            // Clear redirected_backing on the resource since we no
            // longer have a valid backing — the old one was just
            // released and the new allocate failed.
            if let Some(w) = state.resources.window_mut(window) {
                w.redirected_backing = None;
            }
            return;
        }
    };
    if let Some(w) = state.resources.window_mut(window) {
        w.redirected_backing = Some(crate::resources::RedirectedBacking {
            host_pixmap: new_backing,
            width: new_width,
            height: new_height,
            depth,
        });
    }
    // Compatibility retarget: existing NameWindowPixmap aliases on this
    // window must follow the new backing + geometry, or compositors can
    // keep sampling the pre-resize backing for seconds after the frame
    // has already resized (observed with picom under openbox: white
    // right/bottom strip and frozen btop content). Keep the backend
    // alias-registry balanced by moving one hold per alias from OLD to
    // NEW.
    let aliases_to_retarget = state
        .resources
        .window(window)
        .map(|w| w.composite_named_pixmaps.clone())
        .unwrap_or_default();
    for alias in &aliases_to_retarget {
        if let Err(err) = backend.retain_backing_storage(origin, new_backing) {
            log::warn!(
                "rotate_redirected_backing_on_resize(0x{:x}): retain NEW alias backing 0x{:x} failed: {err}",
                window.0,
                new_backing.as_raw(),
            );
        }
        if let Err(err) = backend.drop_backing_storage(origin, alias.host_pixmap) {
            log::warn!(
                "rotate_redirected_backing_on_resize(0x{:x}): drop OLD alias backing 0x{:x} failed: {err}",
                window.0,
                alias.host_pixmap.as_raw(),
            );
        }
        let _ = state
            .resources
            .set_pixmap_host_xid(alias.client_pixmap, new_backing);
        let _ = state.resources.update_pixmap_geometry(
            alias.client_pixmap,
            new_width,
            new_height,
            depth,
        );
        let glx_pixmaps: Vec<u32> = state
            .glx_drawables
            .iter()
            .filter(|(_, d)| d.x_drawable == alias.client_pixmap.0)
            .map(|(xid, _)| *xid)
            .collect();
        for glx_xid in glx_pixmaps {
            retarget_glx_pixmap_export(state, backend, glx_xid, new_backing.as_raw());
        }
    }
    if let Some(w) = state.resources.window_mut(window) {
        for alias in &mut w.composite_named_pixmaps {
            alias.host_pixmap = new_backing;
            alias.width = new_width;
            alias.height = new_height;
        }
    }

    // compCopyWindow analog: carry pre-resize bits from OLD into NEW
    // for the overlap rect. Xorg does this via `compCopyWindow`
    // (composite/compwindow.c:376-388) after `compReallocPixmap`
    // (compalloc.c:680-712). Without it, any compositor that re-Names
    // the post-resize backing (marco on mate-panel-top during the
    // 25→28-px grow) reads an empty buffer and the tray icons that
    // were painted into the pre-resize backing vanish.
    //
    // Storage-alive contract: OLD's backend storage must remain
    // readable across this call. `release_redirected_backing` above
    // only frees when `alias_registry.decref` returns true (refcount
    // hits 0). NameWindowPixmap aliases keep it alive through this
    // path; the v2 backend's lifecycle for the no-alias case is
    // tightened separately.
    //
    // #143: the copy carries CONTENT only. Since #133 the ring lives
    // INSIDE the storage at `(0,0)..(bw,bw)` and the allocate above has
    // already painted NEW's ring at the NEW extent; copying the full
    // `min(old, new)` box from OLD's origin would paint over it — and on
    // a SHRINK that box is the whole new backing, so NEW's right/bottom
    // ring columns would get OLD's *interior* pixels. Xorg reaches the
    // same end state from the other side: `compCopyWindow` copies first
    // and `compSetPixmap` queues `compRepaintBorder` afterwards
    // (../xserver/composite/compwindow.c:137-139), so the ring is always
    // the freshly painted one. Each backing uses its allocation-time
    // border inset; a border-width change moves the content between them.
    let old_inset = old_border_width.saturating_mul(2);
    let new_inset = border_width.saturating_mul(2);
    let copy_w = old_width
        .saturating_sub(old_inset)
        .min(new_width.saturating_sub(new_inset));
    let copy_h = old_height
        .saturating_sub(old_inset)
        .min(new_height.saturating_sub(new_inset));
    let old_content_origin = i16::try_from(old_border_width).unwrap_or(i16::MAX);
    let content_origin = i16::try_from(border_width).unwrap_or(i16::MAX);
    // Storage preservation has no client GC, just like Present's copy.
    let copy_gc = crate::backend::DrawState::default();
    if copy_w > 0
        && copy_h > 0
        && let Err(err) = backend
            .apply_clip_state(origin, &copy_gc.clip)
            .and_then(|()| backend.apply_draw_state(origin, &copy_gc))
            .and_then(|()| {
                backend.copy_area(
                    origin,
                    old_backing.as_raw(),
                    new_backing.as_raw(),
                    old_content_origin,
                    old_content_origin,
                    content_origin,
                    content_origin,
                    copy_w,
                    copy_h,
                )
            })
    {
        log::warn!(
            "rotate_redirected_backing_on_resize(0x{:x}): \
             copy_area(OLD=0x{:x} → NEW=0x{:x}, {copy_w}x{copy_h}) failed: {err}",
            window.0,
            old_backing.as_raw(),
            new_backing.as_raw(),
        );
    }

    // Drop the rotate-scoped retain we took before release. If no
    // other holds remain (no NameWindowPixmap aliases), this is the
    // final ref and OLD's storage is freed here.
    if let Err(err) = backend.drop_backing_storage(origin, old_backing) {
        log::warn!(
            "rotate_redirected_backing_on_resize: drop_backing_storage(0x{:x}) failed: {err}",
            old_backing.as_raw()
        );
    }

    if let Some(mode) = effective_redirect_mode_for_window(state, window) {
        // See activate_redirect_backing_for: B always scene-
        // participating so the post-6ffd370 scene's damage
        // harvest sees paints through `redirected_target`.
        let window_participating = matches!(mode, crate::server::CompositeRedirectMode::Automatic);
        if let Err(err) =
            backend.set_window_scene_participation(origin, host_window, window_participating)
        {
            log::warn!(
                "rotate_redirected_backing_on_resize(0x{:x}): \
                 set_window_scene_participation({window_participating}) failed: {err}",
                window.0
            );
        }
        if let Err(err) = backend.set_backing_scene_participation(origin, new_backing, true) {
            log::warn!(
                "rotate_redirected_backing_on_resize(0x{:x}): \
                 set_backing_scene_participation(true) failed: {err}",
                window.0
            );
        }
    }

    // #143 — report protocol DAMAGE over the ring the reallocate above
    // just repainted, at the NEW geometry.
    //
    // This is the realloc path only, and that is exactly Xorg's gate:
    // `compReallocPixmap` allocates a new pixmap only when the BORDERED
    // extent differs (`pix_w != pOld->drawable.width || pix_h !=
    // pOld->drawable.height`, `../xserver/composite/compalloc.c:698`,
    // with `pix_w = w + (bw << 1)`), and only that branch calls
    // `compSetPixmap(pWin, pNew, bw)` (`:700`). `compSetPixmapVisitWindow`
    // then queues `compRepaintBorder` whenever `bw != 0`
    // (`../xserver/composite/compwindow.c:137-139`), which subtracts
    // `winSize` from `borderClip` and `PaintWindow(..., PW_BORDER)`s the
    // difference (`:113-117`). That paint is an ordinary `PolyFillRect`
    // on the window's pixmap (`../xserver/mi/miexpose.c:558`), so
    // `damagePolyFillRect` (`../xserver/miext/damage/damage.c:1194`)
    // reports it. The same-extent branch (`compalloc.c:702-705`) keeps
    // `pOld`, never calls `compSetPixmap` and therefore reports nothing —
    // which is our `redirected_backing_can_fit` early-return above,
    // deliberately an EXACT extent match (`kms/render/backend.rs:20563`).
    //
    // Ordering matches Xorg's too: `compCopyWindow` carries the bits
    // across first and the border repaint is a WorkProc that runs after,
    // so the ring is always the freshly painted one.
    //
    // We only damage the NEW ring. The region the OLD ring vacated on a
    // shrink lies outside the window's new outer extent, i.e. in the
    // PARENT's area, and Xorg reports it against the parent, never
    // against the shrinking window: `miComputeClips` puts the vacated
    // area into `pParent->valdata->after.exposed`
    // (`../xserver/mi/mivaltree.c:453-460`) and
    // `miHandleValidateExposures` hands it to `miWindowExposures`
    // (`../xserver/mi/miwindow.c:226`), which paints the PARENT's
    // background over it (`../xserver/mi/miexpose.c:387`) — a GC op on
    // the parent's drawable, so the damage lands on the parent.
    //
    // No-op for an unbordered window, for the root and for a
    // non-viewable one (the gate lives in `accumulate_damage_to_state`).
    let _dropped = accumulate_damage_border_to_state(state, window);
}

/// A window's outer (border-inclusive) rect in its parent's content
/// space.
fn outer_rect(
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
) -> x11::xfixes::RegionRect {
    let bw2 = border_width.saturating_mul(2);
    x11::xfixes::RegionRect {
        x,
        y,
        width: width.saturating_add(bw2),
        height: height.saturating_add(bw2),
    }
}

/// `window`'s bounding region in its parent's content space for the
/// geometry given: its outer rect, cut to its bounding shape when it has
/// one (the shape is relative to its content origin). Xorg's
/// `borderSize` (`SetBorderSize`, `dix/window.c:1747-1770`).
fn bounding_in_parent(
    state: &ServerState,
    window: ResourceId,
    (x, y, width, height, border_width): (i16, i16, u16, u16, u16),
) -> Vec<x11::xfixes::RegionRect> {
    let outer = outer_rect(x, y, width, height, border_width);
    let Some(shape) = state
        .shape_windows
        .get(&window)
        .and_then(|s| s.bounding.as_ref())
    else {
        return vec![outer];
    };
    let bw = i16::try_from(border_width).unwrap_or(i16::MAX);
    let shape =
        crate::nested::offset_rects(shape.clone(), x.saturating_add(bw), y.saturating_add(bw));
    crate::nested::intersect_regions(&[outer], &shape)
}

/// [`bounding_in_parent`] at the window's current geometry.
pub(super) fn current_bounding_in_parent(
    state: &ServerState,
    window: ResourceId,
) -> Vec<x11::xfixes::RegionRect> {
    state.resources.window(window).map_or_else(Vec::new, |w| {
        bounding_in_parent(state, window, (w.x, w.y, w.width, w.height, w.border_width))
    })
}

/// The nearest window at or above `window`, short of the root, that is
/// redirected: whose backing `window` draws into.
pub(super) fn redirected_ancestor_or_self(
    state: &ServerState,
    window: ResourceId,
) -> Option<ResourceId> {
    let mut cur = window;
    while cur != crate::resources::ROOT_WINDOW {
        if state.composite_redirects.window_mode(cur).is_some() {
            return Some(cur);
        }
        cur = state.resources.window(cur)?.parent;
    }
    None
}

/// Whether any proper ancestor of `window` is redirected, i.e. the
/// window (when not redirected itself) draws into that ancestor's
/// backing rather than onto the screen.
pub(super) fn has_redirected_ancestor(state: &ServerState, window: ResourceId) -> bool {
    let mut cur = state.resources.window(window).map(|w| w.parent);
    while let Some(ancestor) = cur {
        if ancestor == crate::resources::ROOT_WINDOW {
            return state.composite_redirects.window_mode(ancestor).is_some();
        }
        if state.composite_redirects.window_mode(ancestor).is_some() {
            return true;
        }
        cur = state.resources.window(ancestor).map(|w| w.parent);
    }
    false
}

pub(super) fn effective_redirect_mode_for_window(
    state: &ServerState,
    window: ResourceId,
) -> Option<crate::server::CompositeRedirectMode> {
    state.composite_redirects.window_mode(window)
}
