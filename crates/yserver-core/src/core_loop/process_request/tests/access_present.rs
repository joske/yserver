use super::{
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Creation, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

const CLIENT: ClientId = ClientId(1);
const SERVER_OWNER: ClientId = ClientId(0);
const WINDOW: ResourceId = ResourceId(0x0010_8101);
const NOTIFY_WINDOW: ResourceId = ResourceId(0x0010_8102);
const SECOND_NOTIFY_WINDOW: ResourceId = ResourceId(0x0010_810D);
const PIXMAP: ResourceId = ResourceId(0x0010_8103);
const VALID_REGION: ResourceId = ResourceId(0x0010_8104);
const UPDATE_REGION: ResourceId = ResourceId(0x0010_8105);
const CRTC: ResourceId = ResourceId(0x0010_8106);
const WAIT_FENCE: ResourceId = ResourceId(0x0010_8107);
const IDLE_FENCE: ResourceId = ResourceId(0x0010_8108);
const ACQUIRE_SYNCOBJ: ResourceId = ResourceId(0x0010_8109);
const RELEASE_SYNCOBJ: ResourceId = ResourceId(0x0010_810A);
const EVENT_ID: ResourceId = ResourceId(0x0010_810B);
const NEW_EVENT_ID: ResourceId = ResourceId(0x0010_810C);
const MISSING_REGION: ResourceId = ResourceId(0x0010_81F0);
const MISSING_PIXMAP: ResourceId = ResourceId(0x0010_81F1);
const MISSING_SYNCOBJ: ResourceId = ResourceId(0x0010_81F2);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

struct PresentAccessFixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

#[derive(Clone)]
struct PixmapArgs {
    window: ResourceId,
    pixmap: ResourceId,
    valid: ResourceId,
    update: ResourceId,
    target_crtc: ResourceId,
    wait_fence: ResourceId,
    idle_fence: ResourceId,
    options: u32,
    divisor: u64,
    remainder: u64,
    notifies: Vec<(ResourceId, u32)>,
}

impl Default for PixmapArgs {
    fn default() -> Self {
        Self {
            window: WINDOW,
            pixmap: PIXMAP,
            valid: VALID_REGION,
            update: UPDATE_REGION,
            target_crtc: CRTC,
            wait_fence: WAIT_FENCE,
            idle_fence: IDLE_FENCE,
            options: 0,
            divisor: 0,
            remainder: 0,
            notifies: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct PixmapSyncedArgs {
    window: ResourceId,
    pixmap: ResourceId,
    valid: ResourceId,
    update: ResourceId,
    target_crtc: ResourceId,
    acquire_syncobj: ResourceId,
    release_syncobj: ResourceId,
    acquire_value: u64,
    release_value: u64,
    options: u32,
    divisor: u64,
    remainder: u64,
    notifies: Vec<(ResourceId, u32)>,
}

impl Default for PixmapSyncedArgs {
    fn default() -> Self {
        Self {
            window: WINDOW,
            pixmap: PIXMAP,
            valid: VALID_REGION,
            update: UPDATE_REGION,
            target_crtc: CRTC,
            acquire_syncobj: ACQUIRE_SYNCOBJ,
            release_syncobj: RELEASE_SYNCOBJ,
            acquire_value: 1,
            release_value: 2,
            options: 0,
            divisor: 0,
            remainder: 0,
            notifies: Vec::new(),
        }
    }
}

fn present_access_fixture() -> PresentAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 16,
            height: 16,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: SECOND_NOTIFY_WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 16,
            height: 16,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_window(
        CLIENT,
        CreateWindowRequest {
            depth: 24,
            window: NOTIFY_WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 16,
            height: 16,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(WINDOW)
        .expect("Present window")
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0xA101));
    state
        .resources
        .window_mut(NOTIFY_WINDOW)
        .expect("notify window")
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0xA102));
    state
        .resources
        .window_mut(SECOND_NOTIFY_WINDOW)
        .expect("second notify window")
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0xA104));
    state.resources.create_pixmap(
        CLIENT,
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: crate::resources::ROOT_WINDOW,
            width: 16,
            height: 16,
        },
    );
    assert!(state.resources.set_pixmap_host_xid(
        PIXMAP,
        crate::backend::PixmapHandle::from_raw_for_test(0xA103)
    ));

    for id in [VALID_REGION, UPDATE_REGION] {
        state.xfixes_regions.insert(
            id.0,
            crate::server::XFixesRegion {
                owner: CLIENT,
                region: crate::region::Region::default(),
            },
        );
    }
    for id in [WAIT_FENCE, IDLE_FENCE] {
        state.sync_fences.insert(
            id.0,
            crate::server::SyncFence {
                owner: CLIENT,
                triggered: false,
            },
        );
    }
    for id in [ACQUIRE_SYNCOBJ, RELEASE_SYNCOBJ] {
        assert!(state.resources.register_dri3_syncobj(id, CLIENT));
    }
    let mut backend = RecordingBackend::new();
    backend.seed_dri3_syncobj_for_test(ACQUIRE_SYNCOBJ.0, CLIENT);
    backend.seed_dri3_syncobj_for_test(RELEASE_SYNCOBJ.0, CLIENT);

    state.randr = crate::randr::RandrState::from_outputs(1, vec![test_output(1, CRTC.0, true, 1)]);
    state.present_event_selections.insert(
        EVENT_ID.0,
        crate::server::PresentEventSelection {
            owner: CLIENT,
            window: WINDOW,
            event_mask: 1,
        },
    );

    PresentAccessFixture {
        state,
        peer,
        backend,
    }
}

fn test_output(
    output_id: u32,
    crtc_id: u32,
    connected: bool,
    mode_id: u32,
) -> crate::randr::RandrOutput {
    crate::randr::RandrOutput {
        name: format!("Test-{output_id}"),
        output_id,
        crtc_id,
        mode_id,
        connected,
        x: 0,
        y: 0,
        width: 16,
        height: 16,
        vrefresh: 60,
        timing: None,
        mm_width: 0,
        mm_height: 0,
        mode_ids: vec![mode_id],
        num_preferred: 1,
        pending_transform: Default::default(),
        current_transform: Default::default(),
        rotation: crate::randr::RR_ROTATE_0,
    }
}

fn pixmap_body(args: &PixmapArgs) -> Vec<u8> {
    let mut body = Vec::with_capacity(68 + args.notifies.len() * 8);
    for word in [args.window.0, args.pixmap.0, 7, args.valid.0, args.update.0] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    for word in [
        args.target_crtc.0,
        args.wait_fence.0,
        args.idle_fence.0,
        args.options,
        0,
    ] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&args.divisor.to_le_bytes());
    body.extend_from_slice(&args.remainder.to_le_bytes());
    for (window, serial) in &args.notifies {
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&serial.to_le_bytes());
    }
    body
}

fn pixmap_synced_body(args: &PixmapSyncedArgs) -> Vec<u8> {
    let mut body = Vec::with_capacity(84 + args.notifies.len() * 8);
    for word in [args.window.0, args.pixmap.0, 8, args.valid.0, args.update.0] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    for word in [
        args.target_crtc.0,
        args.acquire_syncobj.0,
        args.release_syncobj.0,
    ] {
        body.extend_from_slice(&word.to_le_bytes());
    }
    body.extend_from_slice(&args.acquire_value.to_le_bytes());
    body.extend_from_slice(&args.release_value.to_le_bytes());
    body.extend_from_slice(&args.options.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&args.divisor.to_le_bytes());
    body.extend_from_slice(&args.remainder.to_le_bytes());
    for (window, serial) in &args.notifies {
        body.extend_from_slice(&window.0.to_le_bytes());
        body.extend_from_slice(&serial.to_le_bytes());
    }
    body
}

fn select_input_body(event_id: ResourceId, window: ResourceId, mask: u32) -> Vec<u8> {
    [event_id.0, window.0, mask]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect()
}

fn dispatch_present(fixture: &mut PresentAccessFixture, minor: u8, body: &[u8]) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
        SequenceNumber(1),
        RequestHeader {
            opcode: 145,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("Present request through process_request");
    read_all_available(&mut fixture.peer)
}

fn resource_target(
    id: ResourceId,
    kind: ResourceKind,
    creation: Option<Creation>,
) -> ResourceTarget {
    ResourceTarget {
        id,
        kind,
        creation,
        owner: None,
    }
}

fn resource_calls(calls: &[RecordedExtensionCheck]) -> Vec<RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .cloned()
        .collect()
}

fn assert_resource_call(
    call: &RecordedExtensionCheck,
    target: ResourceTarget,
    mode: AccessMode,
    minor: u8,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
) {
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, CLIENT);
    assert_eq!(call.target, Some(target));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(call.request, Some(RequestOpcodes { major: 145, minor }));
    assert_eq!(call.owner, owner);
    assert_eq!(call.target_owner, target_owner);
}

fn assert_resource_calls(
    actual: &[RecordedExtensionCheck],
    expected: &[(
        ResourceTarget,
        AccessMode,
        Option<ClientId>,
        Option<ClientId>,
    )],
    minor: u8,
) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "resource checks: actual={actual:?}, expected={expected:?}"
    );
    for (call, (target, mode, owner, target_owner)) in actual.iter().zip(expected) {
        assert_resource_call(call, *target, *mode, minor, *owner, *target_owner);
    }
}

fn assert_policy_error(bytes: &[u8], minor: u8) {
    assert_eq!(bytes.len(), 32, "one X error packet: {bytes:02x?}");
    assert_eq!(bytes[0], 0, "error packet");
    assert_eq!(bytes[1], DENY_ERROR.code);
    assert_eq!(u16::from_le_bytes(bytes[2..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        DENY_ERROR.value,
        "policy error is forwarded verbatim"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], 145);
}

fn assert_present_error(bytes: &[u8], code: u8, value: u32, minor: u8) {
    let mut expected = vec![0, code];
    expected.extend_from_slice(&1u16.to_le_bytes());
    expected.extend_from_slice(&value.to_le_bytes());
    expected.extend_from_slice(&u16::from(minor).to_le_bytes());
    expected.push(145);
    expected.extend_from_slice(&[0; 21]);
    assert_eq!(bytes, expected, "Present request {minor} error packet");
}

fn assert_present_msc_map_eq(
    actual: &std::collections::HashMap<u32, crate::server::PresentWindowMsc>,
    expected: &std::collections::HashMap<u32, crate::server::PresentWindowMsc>,
) {
    assert_eq!(actual.len(), expected.len());
    for (window, expected_clock) in expected {
        let actual_clock = actual.get(window).expect("same Present window clock");
        assert_eq!(actual_clock.last_crtc, expected_clock.last_crtc);
        assert_eq!(actual_clock.last_crtc_epoch, expected_clock.last_crtc_epoch);
        assert_eq!(actual_clock.msc_offset, expected_clock.msc_offset);
        assert_eq!(actual_clock.last_raw_msc, expected_clock.last_raw_msc);
    }
}

#[derive(Clone)]
struct DenyRow {
    label: &'static str,
    minor: u8,
    body: Vec<u8>,
    target: ResourceTarget,
    mode: AccessMode,
    owner: Option<ClientId>,
    target_owner: Option<ClientId>,
}

fn deny_rows() -> Vec<DenyRow> {
    use yserver_protocol::x11::present;

    let pixmap_row = |label, id, kind, mode, body| DenyRow {
        label,
        minor: present::PIXMAP,
        body: pixmap_body(&body),
        target: resource_target(id, kind, None),
        mode,
        owner: if kind == ResourceKind::RandrCrtc {
            Some(SERVER_OWNER)
        } else {
            Some(CLIENT)
        },
        target_owner: if kind == ResourceKind::RandrCrtc {
            Some(SERVER_OWNER)
        } else {
            Some(CLIENT)
        },
    };
    let synced_row = |label, id, kind, mode, body| DenyRow {
        label,
        minor: present::PIXMAP_SYNCED,
        body: pixmap_synced_body(&body),
        target: resource_target(id, kind, None),
        mode,
        owner: if kind == ResourceKind::RandrCrtc {
            Some(SERVER_OWNER)
        } else {
            Some(CLIENT)
        },
        target_owner: if kind == ResourceKind::RandrCrtc {
            Some(SERVER_OWNER)
        } else {
            Some(CLIENT)
        },
    };
    let mut rows = vec![
        pixmap_row(
            "Pixmap window",
            WINDOW,
            ResourceKind::Window,
            AccessMode::WRITE,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap pixmap",
            PIXMAP,
            ResourceKind::Pixmap,
            AccessMode::READ,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap valid region",
            VALID_REGION,
            ResourceKind::Region,
            AccessMode::READ,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap update region",
            UPDATE_REGION,
            ResourceKind::Region,
            AccessMode::READ,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap CRTC",
            CRTC,
            ResourceKind::RandrCrtc,
            AccessMode::READ,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap wait fence",
            WAIT_FENCE,
            ResourceKind::SyncFence,
            AccessMode::READ,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap idle fence",
            IDLE_FENCE,
            ResourceKind::SyncFence,
            AccessMode::WRITE,
            PixmapArgs::default(),
        ),
        pixmap_row(
            "Pixmap notify window",
            NOTIFY_WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            PixmapArgs {
                notifies: vec![(NOTIFY_WINDOW, 19)],
                ..PixmapArgs::default()
            },
        ),
        pixmap_row(
            "Pixmap second notify window",
            SECOND_NOTIFY_WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            PixmapArgs {
                notifies: vec![(NOTIFY_WINDOW, 19), (SECOND_NOTIFY_WINDOW, 20)],
                ..PixmapArgs::default()
            },
        ),
    ];
    let mut notify_msc = Vec::new();
    notify_msc.extend_from_slice(&WINDOW.0.to_le_bytes());
    notify_msc.extend_from_slice(&3u32.to_le_bytes());
    notify_msc.extend_from_slice(&0u32.to_le_bytes());
    notify_msc.extend_from_slice(&0u64.to_le_bytes());
    notify_msc.extend_from_slice(&0u64.to_le_bytes());
    notify_msc.extend_from_slice(&0u64.to_le_bytes());
    rows.push(DenyRow {
        label: "NotifyMSC window",
        minor: present::NOTIFY_MSC,
        body: notify_msc,
        target: resource_target(WINDOW, ResourceKind::Window, None),
        mode: AccessMode::READ,
        owner: Some(CLIENT),
        target_owner: Some(CLIENT),
    });
    rows.extend([
        DenyRow {
            label: "SelectInput window",
            minor: present::SELECT_INPUT,
            body: select_input_body(NEW_EVENT_ID, WINDOW, 1),
            target: resource_target(WINDOW, ResourceKind::Window, None),
            mode: AccessMode::GET_ATTR,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "SelectInput existing selection",
            minor: present::SELECT_INPUT,
            body: select_input_body(EVENT_ID, WINDOW, 3),
            target: resource_target(EVENT_ID, ResourceKind::PresentEventSelection, None),
            mode: AccessMode::WRITE,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "SelectInput new selection",
            minor: present::SELECT_INPUT,
            body: select_input_body(NEW_EVENT_ID, WINDOW, 1),
            target: resource_target(
                NEW_EVENT_ID,
                ResourceKind::PresentEventSelection,
                Some(Creation {
                    parent: Some(WINDOW),
                    owner: CLIENT,
                }),
            ),
            mode: AccessMode::CREATE,
            owner: None,
            target_owner: None,
        },
        DenyRow {
            label: "QueryCapabilities window",
            minor: present::QUERY_CAPABILITIES,
            body: WINDOW.0.to_le_bytes().to_vec(),
            target: resource_target(WINDOW, ResourceKind::Window, None),
            mode: AccessMode::GET_ATTR,
            owner: Some(CLIENT),
            target_owner: Some(CLIENT),
        },
        DenyRow {
            label: "QueryCapabilities CRTC",
            minor: present::QUERY_CAPABILITIES,
            body: CRTC.0.to_le_bytes().to_vec(),
            target: resource_target(CRTC, ResourceKind::RandrCrtc, None),
            mode: AccessMode::GET_ATTR,
            owner: Some(SERVER_OWNER),
            target_owner: Some(SERVER_OWNER),
        },
    ]);
    rows.extend([
        synced_row(
            "PixmapSynced acquire syncobj",
            ACQUIRE_SYNCOBJ,
            ResourceKind::Dri3Syncobj,
            AccessMode::WRITE,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced release syncobj",
            RELEASE_SYNCOBJ,
            ResourceKind::Dri3Syncobj,
            AccessMode::WRITE,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced window",
            WINDOW,
            ResourceKind::Window,
            AccessMode::WRITE,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced pixmap",
            PIXMAP,
            ResourceKind::Pixmap,
            AccessMode::READ,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced valid region",
            VALID_REGION,
            ResourceKind::Region,
            AccessMode::READ,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced update region",
            UPDATE_REGION,
            ResourceKind::Region,
            AccessMode::READ,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced CRTC",
            CRTC,
            ResourceKind::RandrCrtc,
            AccessMode::READ,
            PixmapSyncedArgs::default(),
        ),
        synced_row(
            "PixmapSynced notify window",
            NOTIFY_WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            PixmapSyncedArgs {
                notifies: vec![(NOTIFY_WINDOW, 20)],
                ..PixmapSyncedArgs::default()
            },
        ),
        synced_row(
            "PixmapSynced second notify window",
            SECOND_NOTIFY_WINDOW,
            ResourceKind::Window,
            AccessMode::GET_ATTR,
            PixmapSyncedArgs {
                notifies: vec![(NOTIFY_WINDOW, 20), (SECOND_NOTIFY_WINDOW, 21)],
                ..PixmapSyncedArgs::default()
            },
        ),
    ]);
    rows
}

#[test]
fn present_resource_denials_return_policy_errors_without_side_effects() {
    // Mutation: omit any row's check or change its mode/kind; the corresponding policy error and recorded target assertion must fail.
    for row in deny_rows() {
        let mut fixture = present_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                row.target.id,
                Some(row.mode),
                DENY_ERROR,
            )));
        let selections_before = fixture.state.present_event_selections.clone();
        let generations_before = fixture.state.present_window_generations.clone();
        let present_msc_before = fixture.state.present_window_msc.clone();

        let bytes = dispatch_present(&mut fixture, row.minor, &row.body);

        assert_eq!(
            bytes.len(),
            32,
            "{} did not return an X policy error: {bytes:02x?}",
            row.label
        );
        assert_policy_error(&bytes, row.minor);
        assert_eq!(
            fixture.state.present_event_selections, selections_before,
            "{} changed selections",
            row.label
        );
        assert_eq!(
            fixture.state.present_window_generations, generations_before,
            "{} changed generations",
            row.label
        );
        assert_present_msc_map_eq(&fixture.state.present_window_msc, &present_msc_before);
        assert!(
            fixture.state.present_pending_exec.is_empty(),
            "{} queued a present",
            row.label
        );
        assert!(
            fixture.state.present_pending_msc.is_empty(),
            "{} queued an MSC",
            row.label
        );
        assert!(
            fixture.state.present_pending_complete.is_empty(),
            "{} queued a completion",
            row.label
        );
        assert!(
            fixture.backend.calls().is_empty(),
            "{} reached the backend",
            row.label
        );
        assert!(
            fixture.backend.enqueued_present_completions.is_empty(),
            "{} enqueued a completion",
            row.label
        );
        let recorded = calls.lock().unwrap();
        let resource_calls = resource_calls(&recorded);
        let last = resource_calls
            .last()
            .unwrap_or_else(|| panic!("{} made no resource check", row.label));
        assert_resource_call(
            last,
            row.target,
            row.mode,
            row.minor,
            row.owner,
            row.target_owner,
        );
    }
}

#[test]
fn present_resource_ask_rows_report_every_target_in_order() {
    // Mutation: omit or reorder a listed resource check; this ordered ASK table must fail.
    use yserver_protocol::x11::present;

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let args = PixmapArgs {
        notifies: vec![(NOTIFY_WINDOW, 19), (SECOND_NOTIFY_WINDOW, 20)],
        ..PixmapArgs::default()
    };
    let _ = dispatch_present(&mut fixture, present::PIXMAP, &pixmap_body(&args));
    let expected = [
        (
            resource_target(WINDOW, ResourceKind::Window, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(PIXMAP, ResourceKind::Pixmap, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(VALID_REGION, ResourceKind::Region, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(UPDATE_REGION, ResourceKind::Region, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(CRTC, ResourceKind::RandrCrtc, None),
            AccessMode::READ,
            Some(SERVER_OWNER),
            Some(SERVER_OWNER),
        ),
        (
            resource_target(WAIT_FENCE, ResourceKind::SyncFence, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(IDLE_FENCE, ResourceKind::SyncFence, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(NOTIFY_WINDOW, ResourceKind::Window, None),
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(SECOND_NOTIFY_WINDOW, ResourceKind::Window, None),
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        ),
    ];
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &expected,
        present::PIXMAP,
    );

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let synced = PixmapSyncedArgs {
        notifies: vec![(NOTIFY_WINDOW, 20), (SECOND_NOTIFY_WINDOW, 21)],
        ..PixmapSyncedArgs::default()
    };
    let _ = dispatch_present(
        &mut fixture,
        present::PIXMAP_SYNCED,
        &pixmap_synced_body(&synced),
    );
    let synced_expected = [
        (
            resource_target(ACQUIRE_SYNCOBJ, ResourceKind::Dri3Syncobj, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(RELEASE_SYNCOBJ, ResourceKind::Dri3Syncobj, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(WINDOW, ResourceKind::Window, None),
            AccessMode::WRITE,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(PIXMAP, ResourceKind::Pixmap, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(VALID_REGION, ResourceKind::Region, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(UPDATE_REGION, ResourceKind::Region, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(CRTC, ResourceKind::RandrCrtc, None),
            AccessMode::READ,
            Some(SERVER_OWNER),
            Some(SERVER_OWNER),
        ),
        (
            resource_target(NOTIFY_WINDOW, ResourceKind::Window, None),
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        ),
        (
            resource_target(SECOND_NOTIFY_WINDOW, ResourceKind::Window, None),
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        ),
    ];
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &synced_expected,
        present::PIXMAP_SYNCED,
    );

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let _ = dispatch_present(&mut fixture, present::NOTIFY_MSC, &notify_msc_body(WINDOW));
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[(
            resource_target(WINDOW, ResourceKind::Window, None),
            AccessMode::READ,
            Some(CLIENT),
            Some(CLIENT),
        )],
        present::NOTIFY_MSC,
    );

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let _ = dispatch_present(
        &mut fixture,
        present::SELECT_INPUT,
        &select_input_body(NEW_EVENT_ID, WINDOW, 1),
    );
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[
            (
                resource_target(WINDOW, ResourceKind::Window, None),
                AccessMode::GET_ATTR,
                Some(CLIENT),
                Some(CLIENT),
            ),
            (
                resource_target(
                    NEW_EVENT_ID,
                    ResourceKind::PresentEventSelection,
                    Some(Creation {
                        parent: Some(WINDOW),
                        owner: CLIENT,
                    }),
                ),
                AccessMode::CREATE,
                None,
                None,
            ),
        ],
        present::SELECT_INPUT,
    );

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let _ = dispatch_present(
        &mut fixture,
        present::SELECT_INPUT,
        &select_input_body(EVENT_ID, WINDOW, 3),
    );
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[
            (
                resource_target(WINDOW, ResourceKind::Window, None),
                AccessMode::GET_ATTR,
                Some(CLIENT),
                Some(CLIENT),
            ),
            (
                resource_target(EVENT_ID, ResourceKind::PresentEventSelection, None),
                AccessMode::WRITE,
                Some(CLIENT),
                Some(CLIENT),
            ),
        ],
        present::SELECT_INPUT,
    );

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let _ = dispatch_present(
        &mut fixture,
        present::QUERY_CAPABILITIES,
        &WINDOW.0.to_le_bytes(),
    );
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[(
            resource_target(WINDOW, ResourceKind::Window, None),
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        )],
        present::QUERY_CAPABILITIES,
    );
}

fn notify_msc_body(window: ResourceId) -> Vec<u8> {
    let mut body = Vec::with_capacity(32);
    body.extend_from_slice(&window.0.to_le_bytes());
    body.extend_from_slice(&11u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body.extend_from_slice(&0u64.to_le_bytes());
    body
}

#[test]
fn present_common_zero_sentinels_are_not_asked() {
    // Mutation: remove any zero-sentinel guard for Region, CRTC, wait Fence, or idle Fence; its seeded XID-0 object must expose the extra check.
    use yserver_protocol::x11::present;

    #[derive(Clone, Copy)]
    enum ZeroSentinel {
        ValidRegion,
        UpdateRegion,
        Crtc,
        WaitFence,
        IdleFence,
    }
    let cases = [
        ("valid region None", ZeroSentinel::ValidRegion),
        ("update region None", ZeroSentinel::UpdateRegion),
        ("CRTC None", ZeroSentinel::Crtc),
        ("wait fence None", ZeroSentinel::WaitFence),
        ("idle fence None", ZeroSentinel::IdleFence),
    ];
    for (label, sentinel) in cases {
        let mut fixture = present_access_fixture();
        fixture.state.xfixes_regions.insert(
            0,
            crate::server::XFixesRegion {
                owner: CLIENT,
                region: crate::region::Region::default(),
            },
        );
        fixture.state.sync_fences.insert(
            0,
            crate::server::SyncFence {
                owner: CLIENT,
                triggered: false,
            },
        );
        fixture
            .state
            .randr
            .outputs
            .push(test_output(2, 0, false, 0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
                Decision::Allow,
                Arc::clone(&calls),
            )));
        let mut args = PixmapArgs::default();
        match sentinel {
            ZeroSentinel::ValidRegion => args.valid = ResourceId(0),
            ZeroSentinel::UpdateRegion => args.update = ResourceId(0),
            ZeroSentinel::Crtc => args.target_crtc = ResourceId(0),
            ZeroSentinel::WaitFence => args.wait_fence = ResourceId(0),
            ZeroSentinel::IdleFence => args.idle_fence = ResourceId(0),
        }
        let _ = dispatch_present(&mut fixture, present::PIXMAP, &pixmap_body(&args));
        let actual = resource_calls(&calls.lock().unwrap());
        let mut expected = vec![
            (
                resource_target(WINDOW, ResourceKind::Window, None),
                AccessMode::WRITE,
                Some(CLIENT),
                Some(CLIENT),
            ),
            (
                resource_target(PIXMAP, ResourceKind::Pixmap, None),
                AccessMode::READ,
                Some(CLIENT),
                Some(CLIENT),
            ),
        ];
        if args.valid.0 != 0 {
            expected.push((
                resource_target(args.valid, ResourceKind::Region, None),
                AccessMode::READ,
                Some(CLIENT),
                Some(CLIENT),
            ));
        }
        if args.update.0 != 0 {
            expected.push((
                resource_target(args.update, ResourceKind::Region, None),
                AccessMode::READ,
                Some(CLIENT),
                Some(CLIENT),
            ));
        }
        if args.target_crtc.0 != 0 {
            expected.push((
                resource_target(args.target_crtc, ResourceKind::RandrCrtc, None),
                AccessMode::READ,
                Some(SERVER_OWNER),
                Some(SERVER_OWNER),
            ));
        }
        if args.wait_fence.0 != 0 {
            expected.push((
                resource_target(args.wait_fence, ResourceKind::SyncFence, None),
                AccessMode::READ,
                Some(CLIENT),
                Some(CLIENT),
            ));
        }
        if args.idle_fence.0 != 0 {
            expected.push((
                resource_target(args.idle_fence, ResourceKind::SyncFence, None),
                AccessMode::WRITE,
                Some(CLIENT),
                Some(CLIENT),
            ));
        }
        assert_resource_calls(&actual, &expected, present::PIXMAP);
        assert!(
            actual
                .iter()
                .all(|call| call.target.is_some_and(|target| target.id != ResourceId(0))),
            "{label} sentinel was asked: {actual:?}"
        );
    }
}

#[test]
fn present_pixmap_denied_notify_window_presents_nothing() {
    // Mutation: check notifies after the domain selection; this assertion must catch the changed remembered MSC domain.
    use yserver_protocol::x11::present;

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            SECOND_NOTIFY_WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));
    let before_msc = fixture.state.present_window_msc.clone();
    let before_generation = fixture.state.present_window_generations.clone();
    let args = PixmapArgs {
        notifies: vec![(NOTIFY_WINDOW, 31), (SECOND_NOTIFY_WINDOW, 32)],
        ..PixmapArgs::default()
    };

    let bytes = dispatch_present(&mut fixture, present::PIXMAP, &pixmap_body(&args));

    assert_policy_error(&bytes, present::PIXMAP);
    assert_present_msc_map_eq(&fixture.state.present_window_msc, &before_msc);
    assert_eq!(fixture.state.present_window_generations, before_generation);
    assert!(
        fixture.state.present_pending_exec.is_empty(),
        "denied notify queued a present"
    );
    assert!(
        fixture.state.present_pending_complete.is_empty(),
        "denied notify queued a completion"
    );
    assert!(
        fixture.backend.calls().iter().all(|call| !matches!(
            call,
            crate::backend::recording::RecordedCall::CopyArea { .. }
        )),
        "denied notify reached backend copy"
    );
    assert!(
        fixture.backend.enqueued_present_completions.is_empty(),
        "denied notify enqueued a completion"
    );
    let resource_calls = resource_calls(&calls.lock().unwrap());
    assert_eq!(
        resource_calls.last().and_then(|call| call.target),
        Some(resource_target(
            SECOND_NOTIFY_WINDOW,
            ResourceKind::Window,
            None
        ))
    );
}

#[test]
fn present_select_input_window_checked_before_bad_match() {
    // Mutation: move SelectInput's window check after the existing event-id owner/window BadMatch test.
    use yserver_protocol::x11::present;

    let mut fixture = present_access_fixture();
    fixture.state.present_event_selections.insert(
        EVENT_ID.0,
        crate::server::PresentEventSelection {
            owner: CLIENT,
            window: NOTIFY_WINDOW,
            event_mask: 1,
        },
    );
    let mut no_policy = present_access_fixture();
    no_policy.state.present_event_selections.insert(
        EVENT_ID.0,
        crate::server::PresentEventSelection {
            owner: CLIENT,
            window: NOTIFY_WINDOW,
            event_mask: 1,
        },
    );
    let no_policy_bytes = dispatch_present(
        &mut no_policy,
        present::SELECT_INPUT,
        &select_input_body(EVENT_ID, WINDOW, 3),
    );
    assert_present_error(
        &no_policy_bytes,
        x11::error::BAD_MATCH,
        EVENT_ID.0,
        present::SELECT_INPUT,
    );

    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));
    let bytes = dispatch_present(
        &mut fixture,
        present::SELECT_INPUT,
        &select_input_body(EVENT_ID, WINDOW, 3),
    );
    assert_policy_error(&bytes, present::SELECT_INPUT);
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[(
            resource_target(WINDOW, ResourceKind::Window, None),
            AccessMode::GET_ATTR,
            Some(CLIENT),
            Some(CLIENT),
        )],
        present::SELECT_INPUT,
    );
}

#[test]
fn query_capabilities_crtc_target_is_asked_with_server_owner() {
    // Mutation: omit the Window-to-CRTC fallback resource check; this server-owner target assertion must fail.
    use yserver_protocol::x11::present;

    let mut fixture = present_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    let bytes = dispatch_present(
        &mut fixture,
        present::QUERY_CAPABILITIES,
        &CRTC.0.to_le_bytes(),
    );

    assert_eq!(bytes.len(), 32, "QueryCapabilities reply");
    assert_eq!(bytes[0], 1, "QueryCapabilities succeeds");
    assert_resource_calls(
        &resource_calls(&calls.lock().unwrap()),
        &[(
            resource_target(CRTC, ResourceKind::RandrCrtc, None),
            AccessMode::GET_ATTR,
            Some(SERVER_OWNER),
            Some(SERVER_OWNER),
        )],
        present::QUERY_CAPABILITIES,
    );
}

#[test]
fn present_mixed_failures_keep_operand_check_precedence() {
    // Mutation: run every resource check after the complete validity pass; each denied-earlier/invalid-later pair must stop returning the policy error.
    use yserver_protocol::x11::{error, present};

    struct MixedCase {
        label: &'static str,
        minor: u8,
        body: Vec<u8>,
        denied_id: ResourceId,
        denied_mode: AccessMode,
        no_policy_code: u8,
        no_policy_value: u32,
    }
    let pixmap = |args: PixmapArgs, denied_id, denied_mode, code, value, label| MixedCase {
        label,
        minor: present::PIXMAP,
        body: pixmap_body(&args),
        denied_id,
        denied_mode,
        no_policy_code: code,
        no_policy_value: value,
    };
    let synced = |args: PixmapSyncedArgs, denied_id, denied_mode, code, value, label| MixedCase {
        label,
        minor: present::PIXMAP_SYNCED,
        body: pixmap_synced_body(&args),
        denied_id,
        denied_mode,
        no_policy_code: code,
        no_policy_value: value,
    };
    let cases = [
        pixmap(
            PixmapArgs {
                update: MISSING_REGION,
                ..PixmapArgs::default()
            },
            VALID_REGION,
            AccessMode::READ,
            crate::core_loop::process_request::XFIXES_BAD_REGION,
            MISSING_REGION.0,
            "valid Region denial precedes missing update Region",
        ),
        pixmap(
            PixmapArgs {
                pixmap: MISSING_PIXMAP,
                ..PixmapArgs::default()
            },
            WINDOW,
            AccessMode::WRITE,
            error::BAD_DRAWABLE,
            MISSING_PIXMAP.0,
            "window denial precedes missing pixmap",
        ),
        synced(
            PixmapSyncedArgs {
                update: MISSING_REGION,
                ..PixmapSyncedArgs::default()
            },
            VALID_REGION,
            AccessMode::READ,
            crate::core_loop::process_request::XFIXES_BAD_REGION,
            MISSING_REGION.0,
            "PixmapSynced valid Region denial precedes missing update Region",
        ),
        synced(
            PixmapSyncedArgs {
                pixmap: MISSING_PIXMAP,
                ..PixmapSyncedArgs::default()
            },
            WINDOW,
            AccessMode::WRITE,
            error::BAD_DRAWABLE,
            MISSING_PIXMAP.0,
            "PixmapSynced window denial precedes missing pixmap",
        ),
        pixmap(
            PixmapArgs {
                options: 0x8000_0000,
                ..PixmapArgs::default()
            },
            CRTC,
            AccessMode::READ,
            error::BAD_VALUE,
            0x8000_0000,
            "CRTC denial precedes invalid options",
        ),
        pixmap(
            PixmapArgs {
                idle_fence: IDLE_FENCE,
                options: 0x8000_0000,
                ..PixmapArgs::default()
            },
            IDLE_FENCE,
            AccessMode::WRITE,
            error::BAD_VALUE,
            0x8000_0000,
            "idle fence denial precedes invalid options",
        ),
        synced(
            PixmapSyncedArgs {
                release_syncobj: MISSING_SYNCOBJ,
                ..PixmapSyncedArgs::default()
            },
            ACQUIRE_SYNCOBJ,
            AccessMode::WRITE,
            error::BAD_VALUE,
            MISSING_SYNCOBJ.0,
            "acquire syncobj denial precedes invalid release syncobj",
        ),
        synced(
            PixmapSyncedArgs {
                divisor: 2,
                remainder: 2,
                ..PixmapSyncedArgs::default()
            },
            CRTC,
            AccessMode::READ,
            error::BAD_VALUE,
            2,
            "PixmapSynced CRTC denial precedes invalid remainder",
        ),
    ];

    for case in cases {
        let mut no_policy = present_access_fixture();
        let no_policy_bytes = dispatch_present(&mut no_policy, case.minor, &case.body);
        assert_present_error(
            &no_policy_bytes,
            case.no_policy_code,
            case.no_policy_value,
            case.minor,
        );

        let mut fixture = present_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                case.denied_id,
                Some(case.denied_mode),
                DENY_ERROR,
            )));
        let bytes = dispatch_present(&mut fixture, case.minor, &case.body);
        assert_policy_error(&bytes, case.minor);
        assert!(
            fixture.backend.calls().is_empty(),
            "{} reached backend",
            case.label
        );
        assert!(
            fixture.state.present_window_msc.is_empty(),
            "{} selected a domain",
            case.label
        );
        assert!(
            fixture.state.present_pending_exec.is_empty(),
            "{} queued a present",
            case.label
        );
        let recorded = calls.lock().unwrap();
        let resource_calls = resource_calls(&recorded);
        assert!(
            resource_calls.iter().any(|call| call
                .target
                .is_some_and(|target| target.id == case.denied_id)
                && call.mode == Some(case.denied_mode)),
            "{} omitted the denied operand: {resource_calls:?}",
            case.label
        );
    }
}

#[test]
fn present_unvalidated_wrong_kind_targets_keep_their_existing_behavior() {
    // Mutation: make optional target checks resolve ids across resource kinds; these cases must remain unasked and byte-identical.
    use yserver_protocol::x11::present;

    let query_without_policy = {
        let mut fixture = present_access_fixture();
        dispatch_present(
            &mut fixture,
            present::QUERY_CAPABILITIES,
            &MISSING_REGION.0.to_le_bytes(),
        )
    };
    let mut query_with_policy = present_access_fixture();
    let query_calls = Arc::new(Mutex::new(Vec::new()));
    query_with_policy
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&query_calls),
        )));
    let query_bytes = dispatch_present(
        &mut query_with_policy,
        present::QUERY_CAPABILITIES,
        &MISSING_REGION.0.to_le_bytes(),
    );
    assert_eq!(query_bytes, query_without_policy);
    assert!(
        resource_calls(&query_calls.lock().unwrap()).is_empty(),
        "unresolved QueryCapabilities target was checked"
    );

    // Mutation: restore SelectInput's unconditional Some(window) creation parent; the wrong-kind and unresolved parent rows must fail.
    let select_body = select_input_body(NEW_EVENT_ID, PIXMAP, 1);
    let mut select_no_policy = present_access_fixture();
    let select_without_policy =
        dispatch_present(&mut select_no_policy, present::SELECT_INPUT, &select_body);
    assert!(
        select_without_policy.is_empty(),
        "wrong-kind window was rejected without policy"
    );
    assert!(
        select_no_policy
            .state
            .present_event_selections
            .contains_key(&NEW_EVENT_ID.0)
    );
    let mut select_with_policy = present_access_fixture();
    let select_calls = Arc::new(Mutex::new(Vec::new()));
    select_with_policy
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&select_calls),
        )));
    let select_bytes =
        dispatch_present(&mut select_with_policy, present::SELECT_INPUT, &select_body);
    assert_eq!(select_bytes, select_without_policy);
    assert_resource_calls(
        &resource_calls(&select_calls.lock().unwrap()),
        &[(
            resource_target(
                NEW_EVENT_ID,
                ResourceKind::PresentEventSelection,
                Some(Creation {
                    parent: None,
                    owner: CLIENT,
                }),
            ),
            AccessMode::CREATE,
            None,
            None,
        )],
        present::SELECT_INPUT,
    );

    let unresolved_select_body = select_input_body(NEW_EVENT_ID, MISSING_REGION, 1);
    let mut unresolved_select_no_policy = present_access_fixture();
    let unresolved_select_without_policy = dispatch_present(
        &mut unresolved_select_no_policy,
        present::SELECT_INPUT,
        &unresolved_select_body,
    );
    assert!(
        unresolved_select_without_policy.is_empty(),
        "unresolved window was rejected without policy"
    );
    assert!(
        unresolved_select_no_policy
            .state
            .present_event_selections
            .contains_key(&NEW_EVENT_ID.0)
    );
    let mut unresolved_select_with_policy = present_access_fixture();
    let unresolved_select_calls = Arc::new(Mutex::new(Vec::new()));
    unresolved_select_with_policy
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&unresolved_select_calls),
        )));
    let unresolved_select_bytes = dispatch_present(
        &mut unresolved_select_with_policy,
        present::SELECT_INPUT,
        &unresolved_select_body,
    );
    assert_eq!(unresolved_select_bytes, unresolved_select_without_policy);
    assert_resource_calls(
        &resource_calls(&unresolved_select_calls.lock().unwrap()),
        &[(
            resource_target(
                NEW_EVENT_ID,
                ResourceKind::PresentEventSelection,
                Some(Creation {
                    parent: None,
                    owner: CLIENT,
                }),
            ),
            AccessMode::CREATE,
            None,
            None,
        )],
        present::SELECT_INPUT,
    );

    let notify_args = PixmapArgs {
        notifies: vec![(PIXMAP, 29)],
        ..PixmapArgs::default()
    };
    let notify_body = pixmap_body(&notify_args);
    let notify_without_policy = {
        let mut fixture = present_access_fixture();
        dispatch_present(&mut fixture, present::PIXMAP, &notify_body)
    };
    let mut notify_with_policy = present_access_fixture();
    let notify_calls = Arc::new(Mutex::new(Vec::new()));
    notify_with_policy
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&notify_calls),
        )));
    let notify_bytes = dispatch_present(&mut notify_with_policy, present::PIXMAP, &notify_body);
    assert_eq!(notify_bytes, notify_without_policy);
    let calls = resource_calls(&notify_calls.lock().unwrap());
    assert!(
        calls
            .iter()
            .all(|call| call.target != Some(resource_target(PIXMAP, ResourceKind::Window, None))),
        "Pixmap notify operand of another kind was checked as a Window: {calls:?}"
    );
}
