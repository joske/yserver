use super::*;
use crate::kms::render::{
    backend::{DirectScope, WindowGeometry},
    store::{DrawableId, DrawableKind, Storage},
};
use std::collections::{HashMap, HashSet};
use yserver_core::{
    backend::{Backend, CursorHandle},
    core_loop::message::HostInputEvent,
    resources::ROOT_WINDOW,
    server::ServerState,
};
use yserver_protocol::x11::{ClientId, CreateWindowRequest, ResourceId};

#[derive(Default)]
struct Model {
    sprites: HashMap<u32, DrawableId>,
    frames: HashMap<u32, Vec<u32>>,
    freed: HashSet<u32>,
}

#[derive(Clone, Copy)]
struct TestWindow {
    resource: ResourceId,
    host_xid: u32,
    x: i16,
    y: i16,
}

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(len).expect("nonempty range"))
            .expect("index fits usize")
    }
}

fn sprite_cursor(b: &mut KmsBackend, m: &mut Model) -> u32 {
    let xid = test_cursor(b);
    let sprite_xid = b.core.next_host_xid();
    let sprite = b
        .store
        .allocate(
            sprite_xid,
            DrawableKind::Pixmap,
            32,
            false,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 4,
                    height: 4,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("cursor sprite allocation");
    b.cursor_pixmaps.insert(xid, sprite);
    m.sprites.insert(xid, sprite);
    xid
}

fn create_animation(b: &mut KmsBackend, m: &mut Model, sources: &[u32]) -> u32 {
    let frames: Vec<_> = sources
        .iter()
        .map(|source| (CursorHandle::from_raw(*source).expect("cursor xid"), 10))
        .collect();
    let handle = Backend::create_anim_cursor(b, None, &frames)
        .expect("create animation")
        .expect("animation handle");
    let xid = handle.as_raw();
    m.frames.insert(xid, sources.to_vec());
    xid
}

fn free_cursor(b: &mut KmsBackend, m: &mut Model, xid: u32) {
    Backend::free_cursor(b, None, xid).expect("free cursor");
    assert!(m.freed.insert(xid), "a cursor is freed at most once");
}

fn recount(b: &KmsBackend) -> HashMap<u32, u32> {
    let mut counts = HashMap::new();
    for geom in b.windows.values() {
        if let Some(xid) = geom.cursor {
            *counts.entry(xid).or_insert(0) += 1;
        }
    }
    for anim in b.anim_cursor_records.values() {
        for frame in &anim.frames {
            *counts.entry(frame.source).or_insert(0) += 1;
        }
    }
    counts
}

fn assert_invariants(b: &KmsBackend, m: &Model, reached_reclamation_point: bool) {
    let counts = recount(b);
    assert_eq!(
        b.cursor_refs, counts,
        "cursor refs must match a full recount"
    );

    let mut referenced: HashSet<u32> = counts.keys().copied().collect();
    referenced.extend(scalar_references(b));

    for xid in referenced.intersection(&model_xids(m)) {
        assert!(
            b.cursor_records.contains_key(xid),
            "referenced cursor {xid:#x} lost its record"
        );
        if let Some(sources) = m.frames.get(xid) {
            let anim = b
                .anim_cursor_records
                .get(xid)
                .unwrap_or_else(|| panic!("referenced animation {xid:#x} lost its frames"));
            let actual_sources: Vec<_> = anim.frames.iter().map(|frame| frame.source).collect();
            assert_eq!(actual_sources, *sources, "animation frame list changed");
            for frame in &anim.frames {
                let sprite = m
                    .sprites
                    .get(&frame.source)
                    .copied()
                    .expect("animation source has a model sprite");
                assert_eq!(frame.pixmap, Some(sprite), "frame sprite alias changed");
                assert!(
                    b.store.get(sprite).is_some(),
                    "frame sprite {sprite:?} was reclaimed early"
                );
            }
            let canonical = b
                .cursor_pixmaps
                .get(xid)
                .expect("animation canonical sprite alias");
            assert!(
                sources
                    .iter()
                    .any(|source| m.sprites.get(source) == Some(canonical)),
                "animation canonical sprite is not one of its source sprites"
            );
        } else if let Some(sprite) = m.sprites.get(xid) {
            assert_eq!(b.cursor_pixmaps.get(xid), Some(sprite));
            assert!(
                b.store.get(*sprite).is_some(),
                "static cursor sprite {sprite:?} was reclaimed early"
            );
        }
    }

    if reached_reclamation_point {
        for xid in &m.freed {
            if referenced.contains(xid) {
                continue;
            }
            assert!(
                !b.cursor_records.contains_key(xid),
                "record leaked for {xid:#x}"
            );
            assert!(
                !b.cursor_pixmaps.contains_key(xid),
                "sprite alias leaked for {xid:#x}"
            );
            assert!(
                !b.anim_cursor_records.contains_key(xid),
                "animation leaked for {xid:#x}"
            );
            assert!(
                !b.released_cursors.contains(xid),
                "release marker leaked for {xid:#x}"
            );
            if let Some(sprite) = m.sprites.get(xid) {
                assert!(
                    b.store.get(*sprite).is_none(),
                    "freed cursor sprite {sprite:?} remains in the store"
                );
            }
        }
    }
}

fn model_xids(m: &Model) -> HashSet<u32> {
    m.sprites.keys().chain(m.frames.keys()).copied().collect()
}

fn live_cursors(m: &Model) -> Vec<u32> {
    model_xids(m)
        .into_iter()
        .filter(|xid| !m.freed.contains(xid))
        .collect()
}

fn live_static_cursors(m: &Model) -> Vec<u32> {
    m.sprites
        .keys()
        .copied()
        .filter(|xid| !m.freed.contains(xid))
        .collect()
}

fn create_window(
    state: &mut ServerState,
    b: &mut KmsBackend,
    resource: ResourceId,
    x: i16,
    y: i16,
) -> TestWindow {
    state.resources.create_window(
        ClientId(14),
        CreateWindowRequest {
            depth: 24,
            window: resource,
            parent: ROOT_WINDOW,
            x,
            y,
            width: 100,
            height: 100,
            class: 1,
            visual: yserver_core::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    let host_xid = 0x8000_0000 | resource.0;
    state
        .resources
        .window_mut(resource)
        .expect("created core window")
        .host_xid = yserver_core::backend::WindowHandle::from_raw(host_xid);
    let _ = state.resources.map_window(resource);
    b.core.xid_map.insert(host_xid, resource);
    b.window_geom_insert(
        host_xid,
        WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x,
            y,
            width: 100,
            height: 100,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    b.store
        .allocate(
            host_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("window storage");
    b.windows_restructured(state);
    TestWindow {
        resource,
        host_xid,
        x,
        y,
    }
}

fn destroy_window(state: &mut ServerState, b: &mut KmsBackend, window: TestWindow) {
    dispatch_raw(state, b, 4, 0, &window.resource.0.to_le_bytes());
}

fn motion(x: i32, y: i32) -> HostInputEvent {
    HostInputEvent::PointerMotion {
        origin: yserver_core::core_loop::InputOrigin::NestedHost,
        motion_delta: None,
        device_axes: None,
        x,
        y,
        time: 0,
        relative: false,
        dx: 0,
        dy: 0,
    }
}

fn move_pointer(state: &mut ServerState, b: &mut KmsBackend, window: TestWindow) {
    Backend::on_host_input(
        b,
        state,
        motion(i32::from(window.x) + 20, i32::from(window.y) + 20),
    );
}

fn install_deferred_cow_release(b: &mut KmsBackend, cycle: u32) {
    let target_xid = 0x6f00_0000 + cycle * 2;
    let cow_id = b.cow_id.expect("materialized COW storage");
    b.store
        .allocate(
            target_xid,
            DrawableKind::Window,
            24,
            true,
            Storage::for_tests_null(
                ash::vk::Extent2D {
                    width: 100,
                    height: 100,
                },
                ash::vk::Format::B8G8R8A8_UNORM,
            ),
        )
        .expect("direct target storage");
    b.window_geom_insert(
        target_xid,
        WindowGeometry {
            border_width: 0,
            border_pixel: None,
            border_pixmap: None,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            depth: 24,
            mapped: true,
            viewable: true,
            parent: None,
            stack_rank: 0,
            bg_pixel: None,
            bg_pixmap: None,
            cursor: None,
        },
    );
    install_direct_frame_for_target_test(b, target_xid, cow_id, true);
    b.pstate.direct.root_mut().unflip_shadow_ready = true;
    assert!(Backend::prepare_overlay_release(b, None).expect("prepare deferred COW release"));
    Backend::retire_overlay_window(b, None);
    assert!(b.deferred_cow_release);
}

fn scalar_references(b: &KmsBackend) -> HashSet<u32> {
    [
        b.effective_cursor_xid,
        b.default_cursor_xid,
        b.core.active_cursor,
        b.grab_cursor_override,
    ]
    .into_iter()
    .flatten()
    .collect()
}

// Mutation: omit the cursor_ref_drop in window_geom_remove; each seed's prefix
// frees a window cursor and then destroys its last window reference.
#[test]
fn cursor_refs_match_recount_under_random_sequences() {
    for seed in 1..=64 {
        let mut rng = XorShift64(seed);
        let mut state = ServerState::new();
        let _peer = kbd_map_client_id(&mut state, 14);
        let mut b = KmsBackend::for_tests();
        let mut model = Model::default();
        let mut next_resource = 0x0010_4000 + u32::try_from(seed).unwrap() * 0x1000;
        let mut next_slot = 0usize;
        let mut windows = Vec::new();

        let prefix = create_window(&mut state, &mut b, ResourceId(next_resource), 5, 5);
        next_resource += 1;
        windows.push(prefix);
        let prefix_cursor = sprite_cursor(&mut b, &mut model);
        Backend::define_cursor(&mut b, None, prefix.host_xid, prefix_cursor)
            .expect("define prefix cursor");
        free_cursor(&mut b, &mut model, prefix_cursor);
        let before_destroy = b.windows.get(&prefix.host_xid).unwrap().cursor.is_some();
        destroy_window(&mut state, &mut b, prefix);
        windows.retain(|window| window.host_xid != prefix.host_xid);
        assert!(before_destroy);
        assert_invariants(&b, &model, true);

        let probe = create_window(&mut state, &mut b, ResourceId(next_resource), 50, 50);
        next_resource += 1;
        windows.push(probe);
        let probe_cursor = sprite_cursor(&mut b, &mut model);
        Backend::define_cursor(&mut b, None, probe.host_xid, probe_cursor)
            .expect("define pointer probe cursor");
        move_pointer(&mut state, &mut b, probe);
        assert_eq!(b.effective_cursor_xid, Some(probe_cursor));
        assert_invariants(&b, &model, true);

        let mut deferred_cycle = 0;
        for _ in 0..300 {
            let before_effective = b.effective_cursor_xid;
            let mut reached_reclamation_point = false;
            match rng.index(16) {
                0 => {
                    sprite_cursor(&mut b, &mut model);
                }
                1 => {
                    let sources = live_static_cursors(&model);
                    if !sources.is_empty() {
                        let count = 1 + rng.index(4);
                        let frames: Vec<_> = (0..count)
                            .map(|_| sources[rng.index(sources.len())])
                            .collect();
                        create_animation(&mut b, &mut model, &frames);
                    }
                }
                2 => {
                    let resource = ResourceId(next_resource);
                    next_resource += 1;
                    let slot = next_slot % 24;
                    next_slot += 1;
                    let window = create_window(
                        &mut state,
                        &mut b,
                        resource,
                        i16::try_from((slot % 6) * 110 + 5).unwrap(),
                        i16::try_from((slot / 6) * 110 + 5).unwrap(),
                    );
                    windows.push(window);
                }
                3 => {
                    if !windows.is_empty() {
                        let index = rng.index(windows.len());
                        let window = windows.swap_remove(index);
                        reached_reclamation_point = b
                            .windows
                            .get(&window.host_xid)
                            .is_some_and(|geom| geom.cursor.is_some());
                        destroy_window(&mut state, &mut b, window);
                    }
                }
                4 => {
                    let host_window = if rng.index(5) == 0 || windows.is_empty() {
                        b.core.window_id
                    } else {
                        windows[rng.index(windows.len())].host_xid
                    };
                    let cursors = live_cursors(&model);
                    let cursor = if cursors.is_empty() || rng.index(5) == 0 {
                        0
                    } else {
                        cursors[rng.index(cursors.len())]
                    };
                    Backend::define_cursor(&mut b, None, host_window, cursor)
                        .expect("define cursor");
                    reached_reclamation_point = true;
                }
                5 => {
                    let cursors = live_cursors(&model);
                    if !cursors.is_empty() {
                        let xid = cursors[rng.index(cursors.len())];
                        free_cursor(&mut b, &mut model, xid);
                        reached_reclamation_point = true;
                    }
                }
                6 => {
                    let cursors = live_cursors(&model);
                    if cursors.len() >= 2 {
                        let old = cursors[rng.index(cursors.len())];
                        let mut new = cursors[rng.index(cursors.len())];
                        if old == new {
                            new = cursors[(cursors.iter().position(|xid| *xid == old).unwrap()
                                + 1)
                                % cursors.len()];
                        }
                        Backend::replace_cursor(&mut b, None, old, new).expect("replace cursor");
                        reached_reclamation_point = true;
                    }
                }
                7 => {
                    let cursors = live_cursors(&model);
                    if !cursors.is_empty() {
                        let xid = cursors[rng.index(cursors.len())];
                        Backend::set_grab_cursor(&mut b, None, Some(xid)).expect("set grab cursor");
                        reached_reclamation_point = true;
                    }
                }
                8 => {
                    Backend::set_grab_cursor(&mut b, None, None).expect("clear grab cursor");
                    reached_reclamation_point = true;
                }
                9 => {
                    if !windows.is_empty() {
                        let window = windows[rng.index(windows.len())];
                        move_pointer(&mut state, &mut b, window);
                    }
                }
                10 => {
                    if b.cow_id.is_none() {
                        Backend::get_overlay_window(&mut b, None).expect("get overlay window");
                    }
                }
                11 => {
                    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
                    let cursors = live_cursors(&model);
                    if b.cow_id.is_some() && !b.deferred_cow_release && !cursors.is_empty() {
                        let cursor = cursors[rng.index(cursors.len())];
                        Backend::define_cursor(&mut b, None, cow_xid, cursor)
                            .expect("define COW cursor");
                        reached_reclamation_point = true;
                    }
                }
                12 => {
                    if b.cow_id.is_some() && !b.deferred_cow_release {
                        assert!(
                            Backend::prepare_overlay_release(&mut b, None)
                                .expect("prepare immediate COW release")
                        );
                        Backend::retire_overlay_window(&mut b, None);
                    }
                }
                13 => {
                    if b.cow_id.is_some() && !b.deferred_cow_release {
                        install_deferred_cow_release(&mut b, deferred_cycle);
                        deferred_cycle += 1;
                    }
                }
                14 => {
                    if b.deferred_cow_release {
                        Backend::get_overlay_window(&mut b, None).expect("reacquire deferred COW");
                        b.stop_direct_after_scanout_replaced(
                            DirectScope::Root,
                            "cursor refcount test reacquisition",
                        );
                    }
                }
                15 => {
                    if b.deferred_cow_release {
                        b.stop_direct_after_scanout_replaced(
                            DirectScope::Root,
                            "cursor refcount test deferred teardown",
                        );
                    }
                }
                _ => unreachable!(),
            }
            if b.effective_cursor_xid != before_effective {
                reached_reclamation_point = true;
            }
            assert_invariants(&b, &model, reached_reclamation_point);
        }
    }
}

// Mutation: skip cursor_ref_add for animation frames; duplicate source c1 must count twice.
#[test]
fn duplicate_frame_sources_count_per_frame() {
    let mut b = KmsBackend::for_tests();
    let mut model = Model::default();
    let c1 = sprite_cursor(&mut b, &mut model);
    let c2 = sprite_cursor(&mut b, &mut model);
    let anim = create_animation(&mut b, &mut model, &[c1, c1, c2]);

    assert_eq!(b.cursor_refs.get(&c1), Some(&2));
    assert_eq!(b.cursor_refs.get(&c2), Some(&1));
    free_cursor(&mut b, &mut model, c1);
    free_cursor(&mut b, &mut model, c2);
    free_cursor(&mut b, &mut model, anim);

    assert!(b.cursor_refs.is_empty());
    assert_invariants(&b, &model, true);
}

// Mutation: omit the replaced-entry cursor drop in window_geom_insert; reacquiring
// the deferred COW must clear its previous cursor slot's reference.
#[test]
fn cow_reacquisition_drops_the_overwritten_cursor_reference() {
    let mut b = KmsBackend::for_tests();
    let mut model = Model::default();
    let cow_xid = yserver_core::resources::COMPOSITE_OVERLAY_WINDOW.0;
    Backend::get_overlay_window(&mut b, None).expect("materialize COW");
    let c = sprite_cursor(&mut b, &mut model);
    Backend::define_cursor(&mut b, None, cow_xid, c).expect("define COW cursor");
    free_cursor(&mut b, &mut model, c);

    install_deferred_cow_release(&mut b, 0);
    assert!(b.deferred_cow_release);
    Backend::get_overlay_window(&mut b, None).expect("reacquire COW");
    b.stop_direct_after_scanout_replaced(DirectScope::Root, "cursor refcount test COW replacement");
    assert_eq!(b.cursor_refs.get(&c), None);
    Backend::set_grab_cursor(&mut b, None, None).expect("next reclamation point");
    assert_invariants(&b, &model, true);
}
