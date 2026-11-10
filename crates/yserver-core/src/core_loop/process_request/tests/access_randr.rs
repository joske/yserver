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
