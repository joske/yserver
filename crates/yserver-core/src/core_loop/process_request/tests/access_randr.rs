use super::{
    access_resources::{resource_request, words},
    extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy},
    *,
};
use crate::access::{
    AccessMode, Decision, DenyError, RequestOpcodes, ResourceKind, ResourceTarget,
};

const CLIENT: ClientId = ClientId(1);
const PIXMAP: ResourceId = ResourceId(0x0010_8A01);
const DENY_ERROR: DenyError = DenyError {
    code: x11::error::BAD_ACCESS,
    value: 0xA55A_1234,
};

struct RandrAccessFixture {
    state: ServerState,
    peer: UnixStream,
    backend: RecordingBackend,
}

fn randr_access_fixture() -> RandrAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    RandrAccessFixture {
        state,
        peer,
        backend: RecordingBackend::new(),
    }
}

fn run_request(fixture: &mut RandrAccessFixture, major: u8, minor: u8, body: &[u8]) -> Vec<u8> {
    resource_request(
        &mut fixture.state,
        &mut fixture.backend,
        &mut fixture.peer,
        1,
        major,
        minor,
        body,
    )
}

fn screen_config_body(drawable: ResourceId) -> Vec<u8> {
    let mut body = words(&[drawable.0, 0, 0]);
    body.extend_from_slice(&0_u16.to_le_bytes());
    body.extend_from_slice(&1_u16.to_le_bytes());
    body
}

fn set_screen_size_body(window: ResourceId, width: u16, height: u16) -> Vec<u8> {
    let mut body = words(&[window.0]);
    body.extend_from_slice(&width.to_le_bytes());
    body.extend_from_slice(&height.to_le_bytes());
    body.extend_from_slice(&300_u32.to_le_bytes());
    body.extend_from_slice(&200_u32.to_le_bytes());
    body
}

fn set_monitor_body(window: ResourceId) -> Vec<u8> {
    let mut body = vec![0; 28];
    body[..4].copy_from_slice(&window.0.to_le_bytes());
    body
}

fn output_change_property_body(
    output: ResourceId,
    property: u32,
    prop_type: u32,
    mode: u8,
    format: u8,
    data: &[u8],
) -> Vec<u8> {
    let units = match format {
        8 => data.len(),
        16 => data.len() / 2,
        32 => data.len() / 4,
        _ => 0,
    };
    let mut body = words(&[output.0, property, prop_type]);
    body.extend_from_slice(&[format, mode, 0, 0]);
    body.extend_from_slice(&(units as u32).to_le_bytes());
    body.extend_from_slice(data);
    body
}

fn output_get_property_body(output: ResourceId, property: u32, delete: u8) -> Vec<u8> {
    let mut body = words(&[output.0, property, 0, 0, 1]);
    body.extend_from_slice(&[delete, 0, 0, 0]);
    body
}

fn resource_target(id: ResourceId, kind: ResourceKind) -> ResourceTarget {
    ResourceTarget {
        id,
        kind,
        creation: None,
        owner: None,
    }
}

#[derive(Clone)]
struct AccessRow {
    label: &'static str,
    major: u8,
    minor: u8,
    body: Vec<u8>,
    target: ResourceTarget,
    mode: AccessMode,
}

fn window_row(major: u8, minor: u8, body: Vec<u8>, mode: AccessMode) -> AccessRow {
    AccessRow {
        label: "window request",
        major,
        minor,
        body,
        target: resource_target(ROOT_WINDOW, ResourceKind::Window),
        mode,
    }
}

fn access_rows() -> Vec<AccessRow> {
    use yserver_protocol::x11::{randr as rr, xinerama as xin};

    let root = ROOT_WINDOW;
    vec![
        AccessRow {
            label: "RANDR SetScreenConfig",
            major: 128,
            minor: rr::RR_SET_SCREEN_CONFIG,
            body: screen_config_body(root),
            target: resource_target(root, ResourceKind::Window),
            mode: AccessMode::WRITE,
        },
        window_row(
            128,
            rr::RR_SELECT_INPUT,
            words(&[root.0, 1]),
            AccessMode::RECEIVE,
        ),
        window_row(
            128,
            rr::RR_GET_SCREEN_INFO,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_GET_SCREEN_SIZE_RANGE,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_SET_SCREEN_SIZE,
            set_screen_size_body(root, 800, 600),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_GET_SCREEN_RESOURCES,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_GET_SCREEN_RESOURCES_CURRENT,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_GET_OUTPUT_PRIMARY,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_GET_PROVIDERS,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_GET_MONITORS,
            words(&[root.0, 0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_SET_MONITOR,
            set_monitor_body(root),
            AccessMode::GET_ATTR,
        ),
        window_row(
            128,
            rr::RR_DELETE_MONITOR,
            words(&[root.0, 0]),
            AccessMode::GET_ATTR,
        ),
        window_row(151, xin::GET_STATE, words(&[root.0]), AccessMode::GET_ATTR),
        window_row(
            151,
            xin::GET_SCREEN_COUNT,
            words(&[root.0]),
            AccessMode::GET_ATTR,
        ),
        window_row(
            151,
            xin::GET_SCREEN_SIZE,
            words(&[root.0, 0]),
            AccessMode::GET_ATTR,
        ),
    ]
}

type OutputAccessRow = (AccessRow, Vec<(ResourceTarget, AccessMode)>);

fn set_crtc_config_body(crtc: ResourceId, mode: ResourceId, outputs: &[ResourceId]) -> Vec<u8> {
    let mut body = Vec::with_capacity(24 + outputs.len() * 4);
    body.extend_from_slice(&crtc.0.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&0i16.to_le_bytes());
    body.extend_from_slice(&mode.0.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    for output in outputs {
        body.extend_from_slice(&output.0.to_le_bytes());
    }
    body
}

fn set_crtc_transform_body(crtc: ResourceId, matrix: [i32; 9]) -> Vec<u8> {
    let mut body = Vec::with_capacity(44);
    body.extend_from_slice(&crtc.0.to_le_bytes());
    for cell in matrix {
        body.extend_from_slice(&cell.to_le_bytes());
    }
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body
}

fn set_panning_body(crtc: ResourceId, width: u16, height: u16) -> Vec<u8> {
    let mut body = vec![0; 32];
    body[..4].copy_from_slice(&crtc.0.to_le_bytes());
    body[12..14].copy_from_slice(&width.to_le_bytes());
    body[14..16].copy_from_slice(&height.to_le_bytes());
    body
}

fn crtc_read_rows(crtc: ResourceId) -> Vec<(&'static str, u8, Vec<u8>)> {
    use yserver_protocol::x11::randr as rr;

    vec![
        ("GetCrtcInfo", rr::RR_GET_CRTC_INFO, words(&[crtc.0, 0])),
        (
            "GetCrtcGammaSize",
            rr::RR_GET_CRTC_GAMMA_SIZE,
            words(&[crtc.0]),
        ),
        ("GetCrtcGamma", rr::RR_GET_CRTC_GAMMA, words(&[crtc.0])),
        (
            "SetCrtcTransform",
            rr::RR_SET_CRTC_TRANSFORM,
            set_crtc_transform_body(
                crtc,
                [
                    crate::randr::FIXED_ONE,
                    0,
                    0,
                    0,
                    crate::randr::FIXED_ONE,
                    0,
                    0,
                    0,
                    crate::randr::FIXED_ONE,
                ],
            ),
        ),
        (
            "GetCrtcTransform",
            rr::RR_GET_CRTC_TRANSFORM,
            words(&[crtc.0]),
        ),
        ("GetPanning", rr::RR_GET_PANNING, words(&[crtc.0])),
        (
            "SetPanning",
            rr::RR_SET_PANNING,
            set_panning_body(crtc, 0, 0),
        ),
    ]
}

fn crtc_config_access_rows(fixture: &RandrAccessFixture) -> Vec<OutputAccessRow> {
    use yserver_protocol::x11::randr as rr;

    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let mode = ResourceId(fixture.state.randr.mode_table[0].mode_id);
    let body = set_crtc_config_body(crtc, mode, &[output]);
    let crtc_target = resource_target(crtc, ResourceKind::RandrCrtc);
    let output_target = resource_target(output, ResourceKind::RandrOutput);
    let mode_target = resource_target(mode, ResourceKind::RandrMode);
    let row = |label, target| AccessRow {
        label,
        major: 128,
        minor: rr::RR_SET_CRTC_CONFIG,
        body: body.clone(),
        target,
        mode: AccessMode::SET_ATTR,
    };

    vec![
        (row("RANDR SetCrtcConfig CRTC", crtc_target), Vec::new()),
        (
            row("RANDR SetCrtcConfig output", output_target),
            vec![(crtc_target, AccessMode::SET_ATTR)],
        ),
        (
            row("RANDR SetCrtcConfig mode", mode_target),
            vec![
                (crtc_target, AccessMode::SET_ATTR),
                (output_target, AccessMode::SET_ATTR),
            ],
        ),
    ]
}

fn output_access_rows(fixture: &mut RandrAccessFixture) -> Vec<OutputAccessRow> {
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let mode = ResourceId(fixture.state.randr.mode_table[0].mode_id);
    let property = fixture.state.atoms.intern("ACCESS_TEST_OUTPUT", false).0;
    let prop_type = fixture.state.atoms.intern("ACCESS_TEST_CARDINAL", false).0;
    let output_target = resource_target(output, ResourceKind::RandrOutput);
    let mode_target = resource_target(mode, ResourceKind::RandrMode);
    let row = |label, minor, body, target, mode| {
        (
            AccessRow {
                label,
                major: 128,
                minor,
                body,
                target,
                mode,
            },
            Vec::new(),
        )
    };

    let get_output_info = words(&[output.0, fixture.state.randr.config_timestamp]);
    let mut configure_property = words(&[output.0, property]);
    configure_property.extend_from_slice(&[0, 0, 0, 0]);
    let change_property =
        output_change_property_body(output, property, prop_type, 0, 32, &[1, 0, 0, 0]);
    let add_mode = words(&[output.0, mode.0]);
    let set_primary = words(&[ROOT_WINDOW.0, output.0]);
    vec![
        row(
            "RANDR GetOutputInfo",
            9,
            get_output_info,
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR ListOutputProperties",
            10,
            words(&[output.0]),
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR QueryOutputProperty",
            11,
            words(&[output.0, property]),
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR ConfigureOutputProperty",
            12,
            configure_property,
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR ChangeOutputProperty",
            13,
            change_property,
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR DeleteOutputProperty",
            14,
            words(&[output.0, property]),
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR GetOutputProperty",
            15,
            output_get_property_body(output, property, 0),
            output_target,
            AccessMode::READ,
        ),
        row(
            "RANDR DestroyMode",
            17,
            words(&[mode.0]),
            mode_target,
            AccessMode::DESTROY,
        ),
        row(
            "RANDR AddOutputMode output",
            18,
            add_mode.clone(),
            output_target,
            AccessMode::READ,
        ),
        (
            row(
                "RANDR AddOutputMode mode",
                18,
                add_mode.clone(),
                mode_target,
                AccessMode::USE,
            )
            .0,
            vec![(output_target, AccessMode::READ)],
        ),
        row(
            "RANDR DeleteOutputMode output",
            19,
            add_mode.clone(),
            output_target,
            AccessMode::READ,
        ),
        (
            row(
                "RANDR DeleteOutputMode mode",
                19,
                add_mode,
                mode_target,
                AccessMode::USE,
            )
            .0,
            vec![(output_target, AccessMode::READ)],
        ),
        (
            row(
                "RANDR SetOutputPrimary output",
                30,
                set_primary,
                resource_target(output, ResourceKind::RandrOutput),
                AccessMode::READ,
            )
            .0,
            vec![(
                resource_target(ROOT_WINDOW, ResourceKind::Window),
                AccessMode::GET_ATTR,
            )],
        ),
    ]
}

fn install_allow_policy(
    fixture: &mut RandrAccessFixture,
) -> Arc<Mutex<Vec<RecordedExtensionCheck>>> {
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
    calls
}

fn assert_resource_check(
    call: &RecordedExtensionCheck,
    target: ResourceTarget,
    mode: AccessMode,
    major: u8,
    minor: u8,
) {
    assert_eq!(call.hook, ExtensionHook::Resource);
    assert_eq!(call.client, CLIENT);
    assert_eq!(call.target, Some(target));
    assert_eq!(call.mode, Some(mode));
    assert_eq!(call.request, Some(RequestOpcodes { major, minor }));
    assert_eq!(call.owner, Some(ClientId(0)));
    assert_eq!(call.target_owner, Some(ClientId(0)));
}

fn resource_calls(calls: &[RecordedExtensionCheck]) -> Vec<&RecordedExtensionCheck> {
    calls
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect()
}

fn assert_policy_error(bytes: &[u8], major: u8, minor: u8) {
    assert_eq!(bytes.len(), 32, "expected one policy error: {bytes:02x?}");
    assert_eq!(bytes[0], 0, "expected X error packet");
    assert_eq!(bytes[1], DENY_ERROR.code);
    assert_eq!(u16::from_le_bytes(bytes[2..4].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        DENY_ERROR.value,
        "policy error value is forwarded verbatim"
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], major);
}

#[test]
fn randr_and_xinerama_deny_rows_match_the_site_table() {
    // Mutation: remove the GetScreenInfo resource check; this row must stop returning the policy error.
    for row in access_rows() {
        let mut fixture = randr_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                row.target.id,
                Some(row.mode),
                DENY_ERROR,
            )));

        let bytes = run_request(&mut fixture, row.major, row.minor, &row.body);
        assert_policy_error(&bytes, row.major, row.minor);
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{} resource checks", row.label);
        assert_resource_check(resources[0], row.target, row.mode, row.major, row.minor);
    }
}

#[test]
fn randr_and_xinerama_ask_rows_match_the_site_table() {
    // Mutation: change RANDR SelectInput from RECEIVE to GET_ATTR; this row's mode assertion must fail.
    for row in access_rows() {
        let mut fixture = randr_access_fixture();
        let calls = install_allow_policy(&mut fixture);
        let _bytes = run_request(&mut fixture, row.major, row.minor, &row.body);
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{} resource checks", row.label);
        assert_resource_check(resources[0], row.target, row.mode, row.major, row.minor);
    }
}

#[test]
fn set_screen_config_pixmap_drawable_is_asked_as_pixmap() {
    // Mutation: classify every SetScreenConfig drawable as a Window; this target-kind assertion must fail.
    let mut fixture = randr_access_fixture();
    fixture.state.resources.create_pixmap(
        CLIENT,
        CreatePixmapRequest {
            depth: 24,
            pixmap: PIXMAP,
            drawable: ROOT_WINDOW,
            width: 8,
            height: 8,
        },
    );
    let calls = install_allow_policy(&mut fixture);
    let _bytes = run_request(
        &mut fixture,
        128,
        yserver_protocol::x11::randr::RR_SET_SCREEN_CONFIG,
        &screen_config_body(PIXMAP),
    );
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].client, CLIENT);
    assert_eq!(
        resources[0].target,
        Some(resource_target(PIXMAP, ResourceKind::Pixmap))
    );
    assert_eq!(resources[0].mode, Some(AccessMode::WRITE));
    assert_eq!(
        resources[0].request,
        Some(RequestOpcodes {
            major: 128,
            minor: yserver_protocol::x11::randr::RR_SET_SCREEN_CONFIG,
        })
    );
}

#[test]
fn get_screen_resources_denied_does_not_reprobe() {
    // Mutation: move this check below backend.reprobe_connectors; the recorder must observe a reprobe.
    let mut fixture = randr_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_GET_SCREEN_RESOURCES;
    let bytes = run_request(&mut fixture, 128, minor, &words(&[ROOT_WINDOW.0]));
    assert_policy_error(&bytes, 128, minor);
    assert!(
        fixture
            .backend
            .calls()
            .iter()
            .all(|call| *call != crate::backend::recording::RecordedCall::ReprobeConnectors),
        "denial must precede the connector reprobe"
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::GET_ATTR,
        128,
        minor,
    );
}

#[test]
fn set_monitor_denied_adds_no_monitor_or_root_configure_notify() {
    // Mutation: move SetMonitor's check below insertion and notification; the monitor and event assertions must fail.
    let mut fixture = randr_access_fixture();
    fixture
        .state
        .clients
        .get_mut(&CLIENT.0)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0002_0000);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_SET_MONITOR;
    let name = fixture.state.atoms.intern("ACCESS_TEST_MONITOR", false).0;
    let mut body = set_monitor_body(ROOT_WINDOW);
    body[4..8].copy_from_slice(&name.to_le_bytes());
    let bytes = run_request(&mut fixture, 128, minor, &body);
    assert_policy_error(&bytes, 128, minor);
    assert!(fixture.state.randr_client_monitors.is_empty());
    assert_eq!(
        bytes.len(),
        32,
        "denial must not append root ConfigureNotify"
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::GET_ATTR,
        128,
        minor,
    );
}

#[test]
fn set_screen_size_bad_value_precedes_check() {
    // Mutation: check the raw window XID before parsing SetScreenSize; malformed input must still return BadValue.
    let mut fixture = randr_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE;
    let malformed = words(&[ROOT_WINDOW.0, 800, 600]);
    let bytes = run_request(&mut fixture, 128, minor, &malformed);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], 128);
    assert!(
        resource_calls(&calls.lock().unwrap()).is_empty(),
        "malformed SetScreenSize must not ask the resource policy"
    );
}

#[test]
fn select_input_denial_keeps_existing_subscription() {
    // Mutation: move SelectInput's check below the subscription update; this map assertion must fail.
    let mut fixture = randr_access_fixture();
    let key = (CLIENT.0, ROOT_WINDOW);
    fixture.state.randr_select_masks.insert(key, 0x0002);
    let subscriptions_before = fixture.state.randr_select_masks.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::RECEIVE),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_SELECT_INPUT;
    let bytes = run_request(&mut fixture, 128, minor, &words(&[ROOT_WINDOW.0, 1]));
    assert_policy_error(&bytes, 128, minor);
    assert_eq!(
        fixture.state.randr_select_masks, subscriptions_before,
        "denial must leave RANDR event subscriptions unchanged"
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::RECEIVE,
        128,
        minor,
    );
}

#[test]
fn set_screen_config_denial_keeps_configuration_and_backend_idle() {
    // Mutation: move SetScreenConfig's check below its screen-size update; this state assertion must fail.
    let mut fixture = randr_access_fixture();
    let before = (
        fixture.state.randr.timestamp,
        fixture.state.randr.config_timestamp,
        fixture.state.randr.screen_width,
        fixture.state.randr.screen_height,
        fixture.state.randr.width_mm,
        fixture.state.randr.height_mm,
        fixture.state.randr.outputs[0].rotation,
    );
    let backend_calls_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::WRITE),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_SET_SCREEN_CONFIG;
    let mut body = words(&[ROOT_WINDOW.0, before.0 + 1, before.1]);
    body.extend_from_slice(&0_u16.to_le_bytes());
    body.extend_from_slice(&crate::randr::RR_ROTATE_90.to_le_bytes());
    let bytes = run_request(&mut fixture, 128, minor, &body);
    assert_policy_error(&bytes, 128, minor);
    assert_eq!(
        (
            fixture.state.randr.timestamp,
            fixture.state.randr.config_timestamp,
            fixture.state.randr.screen_width,
            fixture.state.randr.screen_height,
            fixture.state.randr.width_mm,
            fixture.state.randr.height_mm,
            fixture.state.randr.outputs[0].rotation,
        ),
        before,
        "denial must leave the RANDR screen configuration unchanged"
    );
    assert_eq!(
        fixture.backend.calls(),
        backend_calls_before,
        "denial must make no backend calls"
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::WRITE,
        128,
        minor,
    );
}

#[test]
fn set_screen_size_denial_keeps_size_and_backend_idle() {
    // Mutation: move SetScreenSize's check below backend.set_logical_screen_size; this assertion must fail.
    let mut fixture = randr_access_fixture();
    let before = (
        fixture.state.randr.screen_width,
        fixture.state.randr.screen_height,
        fixture.state.randr.width_mm,
        fixture.state.randr.height_mm,
    );
    let backend_calls_before = fixture.backend.calls();
    let (min_width, min_height, max_width, max_height) = fixture.state.randr.screen_size_range();
    let width = before
        .0
        .checked_add(1)
        .expect("screen width has room to grow");
    let height = before
        .1
        .checked_add(1)
        .expect("screen height has room to grow");
    assert!(
        (min_width..=max_width).contains(&width) && (min_height..=max_height).contains(&height),
        "test resize must stay in the RANDR size range"
    );
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_SET_SCREEN_SIZE;
    let bytes = run_request(
        &mut fixture,
        128,
        minor,
        &set_screen_size_body(ROOT_WINDOW, width, height),
    );
    assert_policy_error(&bytes, 128, minor);
    assert!(
        !fixture.backend.calls().iter().any(|call| matches!(
            call,
            crate::backend::recording::RecordedCall::SetLogicalScreenSize { .. }
        )),
        "denial must not call Backend::set_logical_screen_size"
    );
    assert_eq!(
        (
            fixture.state.randr.screen_width,
            fixture.state.randr.screen_height,
            fixture.state.randr.width_mm,
            fixture.state.randr.height_mm,
        ),
        before,
        "denial must leave the RANDR screen size unchanged"
    );
    assert_eq!(
        fixture.backend.calls(),
        backend_calls_before,
        "denial must make no backend calls"
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::GET_ATTR,
        128,
        minor,
    );
}

#[test]
fn delete_monitor_denial_keeps_existing_monitor() {
    // Mutation: move DeleteMonitor's check below removal and notification; the monitor assertion must fail.
    let mut fixture = randr_access_fixture();
    fixture
        .state
        .clients
        .get_mut(&CLIENT.0)
        .unwrap()
        .event_masks
        .insert(ROOT_WINDOW, 0x0002_0000);
    let name = fixture
        .state
        .atoms
        .intern("ACCESS_TEST_DELETE_MONITOR", false)
        .0;
    let monitor = crate::randr::ClientMonitor {
        name,
        primary: true,
        outputs: vec![],
        x: 10,
        y: 20,
        width: 640,
        height: 480,
        width_mm: 200,
        height_mm: 150,
    };
    fixture.state.randr_client_monitors.push(monitor);
    let monitors_before = fixture.state.randr_client_monitors.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ROOT_WINDOW,
            Some(AccessMode::GET_ATTR),
            DENY_ERROR,
        )));

    let minor = yserver_protocol::x11::randr::RR_DELETE_MONITOR;
    let bytes = run_request(&mut fixture, 128, minor, &words(&[ROOT_WINDOW.0, name]));
    assert_eq!(
        fixture.state.randr_client_monitors, monitors_before,
        "denial must preserve the existing client monitor"
    );
    assert_policy_error(&bytes, 128, minor);
    assert_eq!(
        bytes.len(),
        32,
        "denial must not append root ConfigureNotify"
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::GET_ATTR,
        128,
        minor,
    );
}

#[test]
fn randr_output_deny_rows_match_the_site_table() {
    // Mutation: remove the READ check from GetOutputInfo; its row must stop returning the policy error.
    use yserver_protocol::x11::randr as rr;

    let row_count = {
        let mut fixture = randr_access_fixture();
        output_access_rows(&mut fixture).len()
    };
    for row_index in 0..row_count {
        let mut fixture = randr_access_fixture();
        let (row, before) = output_access_rows(&mut fixture).remove(row_index);
        if row.minor == rr::RR_SET_OUTPUT_PRIMARY {
            fixture.state.randr.primary_output = 0;
            fixture
                .state
                .randr_select_masks
                .insert((CLIENT.0, ROOT_WINDOW), rr::NOTIFY_MASK_OUTPUT_CHANGE);
        }
        let primary_before = fixture.state.randr.primary_output;
        let primary_explicit_before = fixture.state.randr_primary_output_explicit;
        let backend_calls_before = fixture.backend.calls();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                row.target.id,
                Some(row.mode),
                DENY_ERROR,
            )));

        let bytes = run_request(&mut fixture, row.major, row.minor, &row.body);
        assert_policy_error(&bytes, row.major, row.minor);
        if row.minor == rr::RR_SET_OUTPUT_PRIMARY {
            assert_eq!(fixture.state.randr.primary_output, primary_before);
            assert_eq!(
                fixture.state.randr_primary_output_explicit,
                primary_explicit_before
            );
            assert_eq!(fixture.backend.calls(), backend_calls_before);
        }
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(
            resources.len(),
            before.len() + 1,
            "{} resource checks",
            row.label
        );
        for (index, (target, mode)) in before.iter().enumerate() {
            assert_resource_check(resources[index], *target, *mode, row.major, row.minor);
        }
        assert_resource_check(
            resources[before.len()],
            row.target,
            row.mode,
            row.major,
            row.minor,
        );
    }
}

#[test]
fn randr_output_ask_rows_match_the_site_table() {
    // Mutation: change ListOutputProperties to ask GET_ATTR; this row's access-mode assertion must fail.
    let mut fixture = randr_access_fixture();
    let rows = output_access_rows(&mut fixture);
    let calls = install_allow_policy(&mut fixture);
    for (row, before) in rows {
        let _bytes = run_request(&mut fixture, row.major, row.minor, &row.body);
        let mut calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        let after = if matches!(row.minor, 18 | 19) && row.target.kind == ResourceKind::RandrOutput
        {
            vec![(
                resource_target(
                    ResourceId(u32::from_le_bytes(row.body[4..8].try_into().unwrap())),
                    ResourceKind::RandrMode,
                ),
                AccessMode::USE,
            )]
        } else {
            Vec::new()
        };
        assert_eq!(
            resources.len(),
            before.len() + 1 + after.len(),
            "{} resource checks",
            row.label
        );
        for (index, (target, mode)) in before.iter().enumerate() {
            assert_resource_check(resources[index], *target, *mode, row.major, row.minor);
        }
        assert_resource_check(
            resources[before.len()],
            row.target,
            row.mode,
            row.major,
            row.minor,
        );
        for (index, (target, mode)) in after.iter().enumerate() {
            assert_resource_check(
                resources[before.len() + 1 + index],
                *target,
                *mode,
                row.major,
                row.minor,
            );
        }
        if row.target.kind == ResourceKind::RandrOutput {
            assert_eq!(
                resources[before.len()].target_owner,
                Some(crate::resources::SERVER_OWNER)
            );
        }
        calls.clear();
        drop(calls);
    }
}

#[test]
fn get_output_property_delete_asks_read_write() {
    // Mutation: validate delete as BOOL before selecting the access mode; raw 2 must still ask READ | WRITE.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let property = fixture.state.atoms.intern("ACCESS_GET_DELETE", false).0;
    let calls = install_allow_policy(&mut fixture);

    for (delete, mode) in [
        (2, AccessMode::READ | AccessMode::WRITE),
        (0, AccessMode::READ),
    ] {
        let bytes = run_request(
            &mut fixture,
            128,
            rr::RR_GET_OUTPUT_PROPERTY,
            &output_get_property_body(output, property, delete),
        );
        if delete == 2 {
            assert_eq!(bytes.len(), 32);
            assert_eq!(bytes[1], x11::error::BAD_VALUE);
            assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 2);
        }
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), if delete == 2 { 1 } else { 2 });
        assert_resource_check(
            resources.last().unwrap(),
            resource_target(output, ResourceKind::RandrOutput),
            mode,
            128,
            rr::RR_GET_OUTPUT_PROPERTY,
        );
    }
}

#[test]
fn delete_output_property_denied_keeps_property() {
    // Mutation: move the DeleteOutputProperty check below removal and notify; both the store and event assertions must fail.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let property = fixture.state.atoms.intern("ACCESS_DELETE_OUTPUT", false).0;
    let prop_type = fixture.state.atoms.intern("ACCESS_DELETE_TYPE", false).0;
    let create = output_change_property_body(output, property, prop_type, 0, 32, &[7, 0, 0, 0]);
    let create_bytes = run_request(&mut fixture, 128, rr::RR_CHANGE_OUTPUT_PROPERTY, &create);
    assert!(create_bytes.is_empty());
    fixture
        .state
        .randr_select_masks
        .insert((CLIENT.0, ROOT_WINDOW), rr::NOTIFY_MASK_OUTPUT_PROPERTY);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            output,
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_DELETE_OUTPUT_PROPERTY,
        &words(&[output.0, property]),
    );
    assert_policy_error(&bytes, 128, rr::RR_DELETE_OUTPUT_PROPERTY);
    assert_eq!(bytes.len(), 32, "denial must not append PropertyNotify");
    assert!(
        fixture.state.randr_output_properties[&output.0]
            .iter()
            .any(|(atom, _)| atom.0 == property)
    );
    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(output, ResourceKind::RandrOutput),
        AccessMode::READ,
        128,
        rr::RR_DELETE_OUTPUT_PROPERTY,
    );
}

#[test]
fn configure_output_property_denied_keeps_metadata() {
    // Mutation: move ConfigureOutputProperty's check below the property metadata update; the snapshot assertions must fail.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let property = fixture
        .state
        .atoms
        .intern("ACCESS_CONFIGURE_DENIED", false)
        .0;
    let prop_type = fixture
        .state
        .atoms
        .intern("ACCESS_CONFIGURE_DENIED_TYPE", false)
        .0;
    let baseline = crate::randr::RandrOutputProperty {
        current: Some(crate::properties::PropertyValue {
            r#type: yserver_protocol::x11::AtomId(prop_type),
            format: crate::properties::PropertyFormat::F32,
            data: 7_u32.to_le_bytes().to_vec(),
        }),
        pending: Some(crate::properties::PropertyValue {
            r#type: yserver_protocol::x11::AtomId(prop_type),
            format: crate::properties::PropertyFormat::F32,
            data: 9_u32.to_le_bytes().to_vec(),
        }),
        is_pending: true,
        range: true,
        immutable: false,
        valid_values: vec![7, 9],
    };
    fixture
        .state
        .randr_output_properties
        .entry(output.0)
        .or_default()
        .push((yserver_protocol::x11::AtomId(property), baseline.clone()));
    fixture
        .state
        .randr_select_masks
        .insert((CLIENT.0, ROOT_WINDOW), rr::NOTIFY_MASK_OUTPUT_PROPERTY);
    let mut body = words(&[output.0, property]);
    body.extend_from_slice(&[0, 0, 0, 0]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            output,
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let backend_calls_before = fixture.backend.calls();
    let bytes = run_request(&mut fixture, 128, rr::RR_CONFIGURE_OUTPUT_PROPERTY, &body);
    let entries = &fixture.state.randr_output_properties[&output.0];
    assert_eq!(entries.len(), 1);
    let stored = &entries[0].1;
    assert_eq!(stored.current, baseline.current);
    assert_eq!(stored.pending, baseline.pending);
    assert_eq!(stored.is_pending, baseline.is_pending);
    assert_eq!(stored.range, baseline.range);
    assert_eq!(stored.immutable, baseline.immutable);
    assert_eq!(stored.valid_values, baseline.valid_values);
    assert_policy_error(&bytes, 128, rr::RR_CONFIGURE_OUTPUT_PROPERTY);
    assert_eq!(fixture.backend.calls(), backend_calls_before);

    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(output, ResourceKind::RandrOutput),
        AccessMode::READ,
        128,
        rr::RR_CONFIGURE_OUTPUT_PROPERTY,
    );
}

#[test]
fn change_output_property_denied_keeps_value() {
    // Mutation: move ChangeOutputProperty's check below the store write and PropertyNotify; the snapshot and stream assertions must fail.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let property = fixture.state.atoms.intern("ACCESS_CHANGE_DENIED", false).0;
    let prop_type = fixture
        .state
        .atoms
        .intern("ACCESS_CHANGE_DENIED_TYPE", false)
        .0;
    let baseline = crate::randr::RandrOutputProperty {
        current: Some(crate::properties::PropertyValue {
            r#type: yserver_protocol::x11::AtomId(prop_type),
            format: crate::properties::PropertyFormat::F32,
            data: 7_u32.to_le_bytes().to_vec(),
        }),
        ..Default::default()
    };
    fixture
        .state
        .randr_output_properties
        .entry(output.0)
        .or_default()
        .push((yserver_protocol::x11::AtomId(property), baseline.clone()));
    fixture
        .state
        .randr_select_masks
        .insert((CLIENT.0, ROOT_WINDOW), rr::NOTIFY_MASK_OUTPUT_PROPERTY);
    let body = output_change_property_body(output, property, prop_type, 0, 32, &[1, 0, 0, 0]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            output,
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let backend_calls_before = fixture.backend.calls();
    let bytes = run_request(&mut fixture, 128, rr::RR_CHANGE_OUTPUT_PROPERTY, &body);
    let entries = &fixture.state.randr_output_properties[&output.0];
    assert_eq!(entries.len(), 1);
    let stored = &entries[0].1;
    assert_eq!(stored.current, baseline.current);
    assert_eq!(stored.pending, baseline.pending);
    assert_eq!(stored.is_pending, baseline.is_pending);
    assert_eq!(stored.range, baseline.range);
    assert_eq!(stored.immutable, baseline.immutable);
    assert_eq!(stored.valid_values, baseline.valid_values);
    assert_policy_error(&bytes, 128, rr::RR_CHANGE_OUTPUT_PROPERTY);
    assert_eq!(fixture.backend.calls(), backend_calls_before);

    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(output, ResourceKind::RandrOutput),
        AccessMode::READ,
        128,
        rr::RR_CHANGE_OUTPUT_PROPERTY,
    );
}

#[test]
fn get_output_property_delete_denied_keeps_property() {
    // Mutation: move GetOutputProperty's check below the delete and PropertyNotify; the store and one-packet assertions must fail.
    use yserver_protocol::x11::{AtomId, randr as rr};

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let property = fixture
        .state
        .atoms
        .intern("ACCESS_GET_DELETE_DENIED", false)
        .0;
    let prop_type = fixture
        .state
        .atoms
        .intern("ACCESS_GET_DELETE_DENIED_TYPE", false)
        .0;
    let baseline = crate::randr::RandrOutputProperty {
        current: Some(crate::properties::PropertyValue {
            r#type: AtomId(prop_type),
            format: crate::properties::PropertyFormat::F32,
            data: 7_u32.to_le_bytes().to_vec(),
        }),
        ..Default::default()
    };
    fixture
        .state
        .randr_output_properties
        .entry(output.0)
        .or_default()
        .push((AtomId(property), baseline.clone()));
    fixture
        .state
        .randr_select_masks
        .insert((CLIENT.0, ROOT_WINDOW), rr::NOTIFY_MASK_OUTPUT_PROPERTY);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            output,
            Some(AccessMode::READ | AccessMode::WRITE),
            DENY_ERROR,
        )));

    let backend_calls_before = fixture.backend.calls();
    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_GET_OUTPUT_PROPERTY,
        &output_get_property_body(output, property, 1),
    );
    let entries = &fixture.state.randr_output_properties[&output.0];
    assert_eq!(entries.len(), 1);
    let stored = &entries[0].1;
    assert_eq!(stored.current, baseline.current);
    assert_eq!(stored.pending, baseline.pending);
    assert_eq!(stored.is_pending, baseline.is_pending);
    assert_eq!(stored.range, baseline.range);
    assert_eq!(stored.immutable, baseline.immutable);
    assert_eq!(stored.valid_values, baseline.valid_values);
    assert_policy_error(&bytes, 128, rr::RR_GET_OUTPUT_PROPERTY);
    assert_eq!(fixture.backend.calls(), backend_calls_before);

    let calls = calls.lock().unwrap();
    assert_resource_check(
        resource_calls(&calls)[0],
        resource_target(output, ResourceKind::RandrOutput),
        AccessMode::READ | AccessMode::WRITE,
        128,
        rr::RR_GET_OUTPUT_PROPERTY,
    );
}

#[test]
fn change_output_property_bad_value_precedes_check() {
    // Mutation: check the output before validating ChangeOutputProperty's mode; denied output must still answer BadValue.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let property = fixture.state.atoms.intern("ACCESS_BAD_MODE", false).0;
    let prop_type = fixture.state.atoms.intern("ACCESS_BAD_MODE_TYPE", false).0;
    let bad_mode = 0xff;
    let body = output_change_property_body(output, property, prop_type, bad_mode, 32, &[]);
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            output,
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let bytes = run_request(&mut fixture, 128, rr::RR_CHANGE_OUTPUT_PROPERTY, &body);
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        u32::from(bad_mode)
    );
    assert!(resource_calls(&calls.lock().unwrap()).is_empty());
}

#[test]
fn output_mode_stubs_unresolved_ids_are_not_asked() {
    // Mutation: substitute a live mode id for the request's unknown mode; the deny-all policy must then reject it.
    let mut fixture = randr_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_all_resources(
            Arc::clone(&calls),
            DENY_ERROR,
        )));
    let unknown_output = ResourceId(0x00de_ad01);
    let unknown_mode = ResourceId(0x00de_ad02);

    for minor in [18, 19] {
        let bytes = run_request(
            &mut fixture,
            128,
            minor,
            &words(&[unknown_output.0, unknown_mode.0]),
        );
        assert!(
            bytes.is_empty(),
            "unsupported mode stub remains a void success"
        );
    }
    assert!(resource_calls(&calls.lock().unwrap()).is_empty());
}

#[test]
fn set_output_primary_none_output_is_not_asked() {
    // Mutation: remove the output != 0 guard before its check; the live id-zero resolver match must be denied.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    fixture.state.randr.outputs[0].output_id = 0;
    fixture.state.randr.primary_output = 0;
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            ResourceId(0),
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_SET_OUTPUT_PRIMARY,
        &words(&[ROOT_WINDOW.0, 0]),
    );
    assert!(bytes.is_empty());
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 1, "None output must not be asked");
    assert_resource_check(
        resources[0],
        resource_target(ROOT_WINDOW, ResourceKind::Window),
        AccessMode::GET_ATTR,
        128,
        rr::RR_SET_OUTPUT_PRIMARY,
    );
}

#[test]
fn randr_crtc_deny_rows_match_site_table() {
    // Mutation: remove the SetCrtcConfig output check; denying that row must stop returning the policy error.
    for row_index in 0..3 {
        let mut fixture = randr_access_fixture();
        let (row, before) = crtc_config_access_rows(&fixture).remove(row_index);
        let randr_before = format!("{:?}", fixture.state.randr);
        let backend_calls_before = fixture.backend.calls();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                row.target.id,
                Some(row.mode),
                DENY_ERROR,
            )));

        let bytes = run_request(&mut fixture, row.major, row.minor, &row.body);
        assert_policy_error(&bytes, row.major, row.minor);
        assert_eq!(format!("{:?}", fixture.state.randr), randr_before);
        assert_eq!(fixture.backend.calls(), backend_calls_before);
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), before.len() + 1, "{} checks", row.label);
        for (index, (target, mode)) in before.iter().enumerate() {
            assert_resource_check(resources[index], *target, *mode, row.major, row.minor);
        }
        assert_resource_check(
            resources[before.len()],
            row.target,
            row.mode,
            row.major,
            row.minor,
        );
    }
}

#[test]
fn randr_crtc_read_ask_rows_match_site_table() {
    // Mutation: remove any new CRTC READ check; its site's recorded-check assertion must fail.
    for (label, minor, _) in crtc_read_rows(ResourceId(0)) {
        let mut fixture = randr_access_fixture();
        let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
        let body = crtc_read_rows(crtc)
            .into_iter()
            .find(|(row, _, _)| *row == label)
            .expect("site row")
            .2;
        let calls = install_allow_policy(&mut fixture);
        let bytes = run_request(&mut fixture, 128, minor, &body);
        if minor == yserver_protocol::x11::randr::RR_SET_CRTC_TRANSFORM {
            assert!(bytes.is_empty(), "{label} has no reply");
        } else {
            assert!(!bytes.is_empty(), "{label} reply");
            assert_eq!(bytes[0], 1, "{label} successful reply marker");
        }
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{label} check count");
        assert_resource_check(
            resources[0],
            resource_target(crtc, ResourceKind::RandrCrtc),
            AccessMode::READ,
            128,
            minor,
        );
    }
}

#[test]
fn randr_crtc_read_deny_rows_precede_later_work() {
    // Mutation: move any new CRTC READ check below later validation or backend work; its denial row must fail.
    use yserver_protocol::x11::randr as rr;

    for (label, minor, mut body) in crtc_read_rows(ResourceId(0x0020_0001)) {
        let mut fixture = randr_access_fixture();
        let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
        body[..4].copy_from_slice(&crtc.0.to_le_bytes());
        if minor == rr::RR_SET_CRTC_TRANSFORM {
            body[4..40].fill(0);
        } else if minor == rr::RR_SET_PANNING {
            body[12..14].copy_from_slice(&1920u16.to_le_bytes());
            body[14..16].copy_from_slice(&1080u16.to_le_bytes());
        }
        let randr_before = format!("{:?}", fixture.state.randr);
        let backend_calls_before = fixture.backend.calls();
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                crtc,
                Some(AccessMode::READ),
                DENY_ERROR,
            )));

        let bytes = run_request(&mut fixture, 128, minor, &body);
        assert_policy_error(&bytes, 128, minor);
        assert_eq!(
            format!("{:?}", fixture.state.randr),
            randr_before,
            "{label} state"
        );
        assert_eq!(
            fixture.backend.calls(),
            backend_calls_before,
            "{label} backend calls"
        );
        assert!(
            fixture.backend.calls().is_empty(),
            "{label} must not call backend"
        );
        let calls = calls.lock().unwrap();
        let resources = resource_calls(&calls);
        assert_eq!(resources.len(), 1, "{label} check count");
        assert_resource_check(
            resources[0],
            resource_target(crtc, ResourceKind::RandrCrtc),
            AccessMode::READ,
            128,
            minor,
        );
    }
}

#[test]
fn randr_crtc_ask_rows_match_site_table() {
    // Mutation: skip the SetCrtcConfig mode check; the ordered three-operand assertion must fail.
    // Mutation: encode an X error marker on success; the reply-marker assertion must fail.
    // Mutation: return RRSetConfigFailed on success; the successful-status assertion must fail.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let first_output = fixture.state.randr.outputs[0].clone();
    let mut second_output = first_output.clone();
    second_output.output_id = first_output.output_id.wrapping_add(1000);
    second_output.name.push_str("-second");
    fixture.state.randr.outputs.push(second_output);
    let crtc = ResourceId(first_output.crtc_id);
    let first_output_id = ResourceId(first_output.output_id);
    let second_output_id = ResourceId(fixture.state.randr.outputs[1].output_id);
    let mode = ResourceId(fixture.state.randr.mode_table[0].mode_id);
    let expected = [
        (
            resource_target(crtc, ResourceKind::RandrCrtc),
            AccessMode::SET_ATTR,
        ),
        (
            resource_target(first_output_id, ResourceKind::RandrOutput),
            AccessMode::SET_ATTR,
        ),
        (
            resource_target(second_output_id, ResourceKind::RandrOutput),
            AccessMode::SET_ATTR,
        ),
        (
            resource_target(mode, ResourceKind::RandrMode),
            AccessMode::SET_ATTR,
        ),
    ];
    let body = set_crtc_config_body(crtc, mode, &[first_output_id, second_output_id]);
    let calls = install_allow_policy(&mut fixture);
    let bytes = run_request(&mut fixture, 128, rr::RR_SET_CRTC_CONFIG, &body);
    assert_eq!(bytes.len(), 32, "successful SetCrtcConfig reply");
    assert_eq!(bytes[0], 1, "successful X reply marker");
    assert_eq!(bytes[1], rr::SET_CONFIG_SUCCESS, "successful RANDR status");
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), expected.len());
    for (call, (target, mode)) in resources.iter().zip(expected) {
        assert_resource_check(call, target, mode, 128, rr::RR_SET_CRTC_CONFIG);
    }
}

#[test]
fn set_crtc_config_denied_output_before_bad_mode() {
    // Mutation: run the whole validation helper before checking outputs; the denied output must win over BadMatch(mode).
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let bad_mode = ResourceId(0xDEAD_BEEF);
    let randr_before = format!("{:?}", fixture.state.randr);
    let backend_calls_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            output,
            Some(AccessMode::SET_ATTR),
            DENY_ERROR,
        )));

    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(crtc, bad_mode, &[output]),
    );
    assert_eq!(format!("{:?}", fixture.state.randr), randr_before);
    assert_eq!(fixture.backend.calls(), backend_calls_before);
    assert_policy_error(&bytes, 128, rr::RR_SET_CRTC_CONFIG);
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2);
    assert_resource_check(
        resources[0],
        resource_target(crtc, ResourceKind::RandrCrtc),
        AccessMode::SET_ATTR,
        128,
        rr::RR_SET_CRTC_CONFIG,
    );
    assert_resource_check(
        resources[1],
        resource_target(output, ResourceKind::RandrOutput),
        AccessMode::SET_ATTR,
        128,
        rr::RR_SET_CRTC_CONFIG,
    );

    let mut no_policy = randr_access_fixture();
    let no_policy_output = ResourceId(no_policy.state.randr.outputs[0].output_id);
    let no_policy_crtc = ResourceId(no_policy.state.randr.outputs[0].crtc_id);
    let bytes = run_request(
        &mut no_policy,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(no_policy_crtc, bad_mode, &[no_policy_output]),
    );
    assert_eq!(bytes.len(), 32);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        bad_mode.0
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(rr::RR_SET_CRTC_CONFIG)
    );
    assert_eq!(bytes[10], 128);
}

#[test]
fn set_crtc_config_denied_crtc_before_arity() {
    // Mutation: check the CRTC after the validation helper; mode=0 plus an output must return BadMatch first.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let randr_before = format!("{:?}", fixture.state.randr);
    let backend_calls_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            crtc,
            Some(AccessMode::SET_ATTR),
            DENY_ERROR,
        )));

    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(crtc, ResourceId(0), &[output]),
    );
    assert_eq!(format!("{:?}", fixture.state.randr), randr_before);
    assert_eq!(fixture.backend.calls(), backend_calls_before);
    assert_policy_error(&bytes, 128, rr::RR_SET_CRTC_CONFIG);
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 1);
    assert_resource_check(
        resources[0],
        resource_target(crtc, ResourceKind::RandrCrtc),
        AccessMode::SET_ATTR,
        128,
        rr::RR_SET_CRTC_CONFIG,
    );
}

#[test]
fn set_crtc_config_denied_makes_no_backend_call() {
    // Mutation: move the mode check below start_crtc_config; no ApplyCrtcConfig call may follow denial.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let output = ResourceId(fixture.state.randr.outputs[0].output_id);
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let mode = ResourceId(fixture.state.randr.mode_table[0].mode_id);
    let randr_before = format!("{:?}", fixture.state.randr);
    let backend_calls_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            mode,
            Some(AccessMode::SET_ATTR),
            DENY_ERROR,
        )));

    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(crtc, mode, &[output]),
    );
    assert_eq!(format!("{:?}", fixture.state.randr), randr_before);
    assert_eq!(fixture.backend.calls(), backend_calls_before);
    assert!(
        fixture.backend.calls().is_empty(),
        "denial must not call backend"
    );
    assert_policy_error(&bytes, 128, rr::RR_SET_CRTC_CONFIG);
}

#[test]
fn set_crtc_config_disable_asks_crtc_only() {
    // Mutation: skip the pre-helper CRTC check; a valid disable must still record exactly its CRTC.
    // Mutation: encode an X error marker on success; the reply-marker assertion must fail.
    // Mutation: return RRSetConfigFailed on success; the successful-status assertion must fail.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let calls = install_allow_policy(&mut fixture);
    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(crtc, ResourceId(0), &[]),
    );
    assert_eq!(bytes.len(), 32, "successful disable reply");
    assert_eq!(bytes[0], 1, "successful X reply marker");
    assert_eq!(bytes[1], rr::SET_CONFIG_SUCCESS, "successful RANDR status");
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 1);
    assert_resource_check(
        resources[0],
        resource_target(crtc, ResourceKind::RandrCrtc),
        AccessMode::SET_ATTR,
        128,
        rr::RR_SET_CRTC_CONFIG,
    );
}

#[test]
fn set_crtc_config_zero_ids_are_not_asked() {
    // Mutation: remove any RANDR CRTC zero-id guard; the matching resolver must not create a policy check.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    fixture.state.randr.outputs[0].output_id = 0;
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let mode = ResourceId(fixture.state.randr.mode_table[0].mode_id);
    let calls = install_allow_policy(&mut fixture);
    let bytes = run_request(
        &mut fixture,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(crtc, mode, &[ResourceId(0)]),
    );
    assert_eq!(bytes.len(), 32);
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 2);
    assert_resource_check(
        resources[0],
        resource_target(crtc, ResourceKind::RandrCrtc),
        AccessMode::SET_ATTR,
        128,
        rr::RR_SET_CRTC_CONFIG,
    );
    assert_resource_check(
        resources[1],
        resource_target(mode, ResourceKind::RandrMode),
        AccessMode::SET_ATTR,
        128,
        rr::RR_SET_CRTC_CONFIG,
    );
    drop(calls);

    let mut zero_crtc = randr_access_fixture();
    zero_crtc.state.randr.outputs[0].crtc_id = 0;
    let calls = install_allow_policy(&mut zero_crtc);
    let bytes = run_request(
        &mut zero_crtc,
        128,
        rr::RR_SET_CRTC_CONFIG,
        &set_crtc_config_body(ResourceId(0), ResourceId(0), &[]),
    );
    assert_eq!(bytes.len(), 32);
    assert!(resource_calls(&calls.lock().unwrap()).is_empty());

    let mut zero_gamma_crtc = randr_access_fixture();
    zero_gamma_crtc.state.randr.outputs[0].crtc_id = 0;
    let calls = install_allow_policy(&mut zero_gamma_crtc);
    let mut gamma_body = vec![0; 14];
    gamma_body[4..6].copy_from_slice(&1u16.to_le_bytes());
    let bytes = run_request(
        &mut zero_gamma_crtc,
        128,
        rr::RR_SET_CRTC_GAMMA,
        &gamma_body,
    );
    assert_eq!(bytes[1], x11::error::BAD_MATCH);
    assert!(resource_calls(&calls.lock().unwrap()).is_empty());

    for (label, minor, body) in crtc_read_rows(ResourceId(0)) {
        let mut zero_crtc = randr_access_fixture();
        zero_crtc.state.randr.outputs[0].crtc_id = 0;
        let calls = install_allow_policy(&mut zero_crtc);
        let bytes = run_request(&mut zero_crtc, 128, minor, &body);
        assert!(resource_calls(&calls.lock().unwrap()).is_empty(), "{label}");
        if minor == yserver_protocol::x11::randr::RR_SET_CRTC_TRANSFORM {
            assert!(bytes.is_empty(), "{label} no-reply request");
        } else {
            assert!(!bytes.is_empty(), "{label} reply");
        }
    }
}

#[test]
fn set_crtc_gamma_denied_before_size_check() {
    // Mutation: move SetCrtcGamma's access check below its size validation; the policy error must beat BadMatch.
    // Mutation: request SET_ATTR instead of READ; a READ-only denial must prevent the backend size query.
    use yserver_protocol::x11::randr as rr;

    let mut fixture = randr_access_fixture();
    let crtc = ResourceId(fixture.state.randr.outputs[0].crtc_id);
    let mut body = vec![0; 14];
    body[..4].copy_from_slice(&crtc.0.to_le_bytes());
    body[4..6].copy_from_slice(&1u16.to_le_bytes());
    let randr_before = format!("{:?}", fixture.state.randr);
    let backend_calls_before = fixture.backend.calls();
    let calls = Arc::new(Mutex::new(Vec::new()));
    fixture
        .state
        .install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            crtc,
            Some(AccessMode::READ),
            DENY_ERROR,
        )));

    let bytes = run_request(&mut fixture, 128, rr::RR_SET_CRTC_GAMMA, &body);
    assert_eq!(format!("{:?}", fixture.state.randr), randr_before);
    assert_eq!(fixture.backend.calls(), backend_calls_before);
    assert_policy_error(&bytes, 128, rr::RR_SET_CRTC_GAMMA);
    let calls = calls.lock().unwrap();
    let resources = resource_calls(&calls);
    assert_eq!(resources.len(), 1);
    assert_resource_check(
        resources[0],
        resource_target(crtc, ResourceKind::RandrCrtc),
        AccessMode::READ,
        128,
        rr::RR_SET_CRTC_GAMMA,
    );

    let mut allowed = randr_access_fixture();
    let allowed_crtc = ResourceId(allowed.state.randr.outputs[0].crtc_id);
    let mut valid_body = vec![0; 8 + 256 * 6];
    valid_body[..4].copy_from_slice(&allowed_crtc.0.to_le_bytes());
    valid_body[4..6].copy_from_slice(&256u16.to_le_bytes());
    install_allow_policy(&mut allowed);
    let bytes = run_request(&mut allowed, 128, rr::RR_SET_CRTC_GAMMA, &valid_body);
    assert!(bytes.is_empty());
    assert!(allowed.backend.calls().contains(
        &crate::backend::recording::RecordedCall::CrtcGammaSize {
            crtc: allowed_crtc.0,
        }
    ));
}
