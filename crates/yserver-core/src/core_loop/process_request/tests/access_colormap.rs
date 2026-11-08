use super::{
    access_resources::{CLIENT, assert_x11_error, window_resource_policy, words},
    *,
};
use crate::access::{AccessMode, Creation, RequestOpcodes, ResourceKind, ResourceTarget};
use std::sync::{Arc, Mutex, atomic::AtomicU32};

const COLORMAP: ResourceId = ResourceId(0x0030_0001);
const NEW_COLORMAP: ResourceId = ResourceId(0x0030_0002);

struct ColormapAccessCase {
    label: &'static str,
    opcode: u8,
    data: u8,
    body: Vec<u8>,
    checks: Vec<(ResourceTarget, AccessMode)>,
    installed_colormaps: Option<Vec<ResourceId>>,
}

type ColormapCheck = (ResourceTarget, AccessMode);
type ColormapRow = (&'static str, u8);

struct ColormapExpected {
    checks: Vec<ColormapCheck>,
    installed: Option<Vec<ResourceId>>,
}

const CREATE_COLORMAP_ROW: ColormapRow = ("CreateColormap", 78);
const COPY_COLORMAP_ROW: ColormapRow = ("CopyColormapAndFree", 80);
const UNINSTALL_COLORMAP_ROW: ColormapRow = ("UninstallColormap", 82);

fn colormap_target(id: ResourceId, creation: Option<Creation>) -> ResourceTarget {
    ResourceTarget {
        owner: None,
        id,
        kind: ResourceKind::Colormap,
        creation,
    }
}

fn cmcase(label: &'static str, opcode: u8, body: Vec<u8>, mode: AccessMode) -> ColormapAccessCase {
    cmrow(
        (label, opcode),
        body,
        expected(vec![(colormap_target(COLORMAP, None), mode)]),
    )
}

fn cmrow(
    (label, opcode): ColormapRow,
    body: Vec<u8>,
    expected: ColormapExpected,
) -> ColormapAccessCase {
    ColormapAccessCase {
        label,
        opcode,
        data: 0,
        body,
        checks: expected.checks,
        installed_colormaps: expected.installed,
    }
}

fn win_case(
    label: &'static str,
    opcode: u8,
    body: Vec<u8>,
    mode: AccessMode,
) -> ColormapAccessCase {
    cmrow(
        (label, opcode),
        body,
        expected(vec![(
            ResourceTarget {
                owner: None,
                id: ROOT_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
            },
            mode,
        )]),
    )
}

fn create_checks() -> ColormapExpected {
    expected(vec![
        (
            ResourceTarget {
                owner: None,
                id: ROOT_WINDOW,
                kind: ResourceKind::Window,
                creation: None,
            },
            AccessMode::GET_ATTR,
        ),
        (
            colormap_target(
                NEW_COLORMAP,
                Some(Creation {
                    parent: Some(ROOT_WINDOW),
                    owner: CLIENT,
                }),
            ),
            AccessMode::CREATE,
        ),
    ])
}

fn copy_checks() -> ColormapExpected {
    expected(vec![
        (
            colormap_target(COLORMAP, None),
            AccessMode::READ | AccessMode::REMOVE,
        ),
        (
            colormap_target(
                NEW_COLORMAP,
                Some(Creation {
                    parent: None,
                    owner: CLIENT,
                }),
            ),
            AccessMode::CREATE,
        ),
    ])
}

fn uninstall_checks() -> ColormapExpected {
    ColormapExpected {
        checks: vec![(colormap_target(COLORMAP, None), AccessMode::UNINSTALL)],
        installed: Some(vec![COLORMAP]),
    }
}

fn expected(checks: Vec<ColormapCheck>) -> ColormapExpected {
    ColormapExpected {
        checks,
        installed: None,
    }
}

fn colormap_access_cases() -> Vec<ColormapAccessCase> {
    use AccessMode as M;

    let mut alloc_color = COLORMAP.0.to_le_bytes().to_vec();
    alloc_color.extend_from_slice(&[0; 8]);
    let alloc_named_color = named_color_body(COLORMAP, b"red");
    let mut alloc_color_cells = COLORMAP.0.to_le_bytes().to_vec();
    alloc_color_cells.extend_from_slice(&[1, 0, 0, 0]);
    let mut alloc_color_planes = COLORMAP.0.to_le_bytes().to_vec();
    alloc_color_planes.extend_from_slice(&[1, 0, 1, 0, 1, 0, 1, 0]);
    let mut free_colors = COLORMAP.0.to_le_bytes().to_vec();
    free_colors.extend_from_slice(&[0; 4]);
    let mut store_named_color = COLORMAP.0.to_le_bytes().to_vec();
    store_named_color.extend_from_slice(&[0; 4]);
    store_named_color.extend_from_slice(&[3, 0, 0, 0]);
    store_named_color.extend_from_slice(b"red\0");
    let mut query_colors = COLORMAP.0.to_le_bytes().to_vec();
    query_colors.extend_from_slice(&[0; 4]);

    let create_body = words(&[
        NEW_COLORMAP.0,
        ROOT_WINDOW.0,
        crate::resources::ROOT_VISUAL.0,
    ]);
    let colormap_word = words(&[COLORMAP.0]);
    let root_word = words(&[ROOT_WINDOW.0]);
    let copy_body = words(&[NEW_COLORMAP.0, COLORMAP.0]);
    let uninstall_body = colormap_word.clone();
    let lookup_body = named_color_body(COLORMAP, b"red");
    let cases = vec![
        cmrow(CREATE_COLORMAP_ROW, create_body, create_checks()),
        cmcase("FreeColormap", 79, colormap_word.clone(), M::DESTROY),
        cmrow(COPY_COLORMAP_ROW, copy_body, copy_checks()),
        cmcase("InstallColormap", 81, colormap_word.clone(), M::INSTALL),
        cmrow(UNINSTALL_COLORMAP_ROW, uninstall_body, uninstall_checks()),
        win_case("ListInstalledColormaps", 83, root_word, M::GET_ATTR),
        cmcase("AllocColor", 84, alloc_color, M::ADD),
        cmcase("AllocNamedColor", 85, alloc_named_color, M::ADD),
        cmcase("AllocColorCells", 86, alloc_color_cells, M::ADD),
        cmcase("AllocColorPlanes", 87, alloc_color_planes, M::ADD),
        cmcase("FreeColors", 88, free_colors, M::REMOVE),
        cmcase("StoreColors", 89, colormap_word, M::WRITE),
        cmcase("StoreNamedColor", 90, store_named_color, M::WRITE),
        cmcase("QueryColors", 91, query_colors, M::READ),
        cmcase("LookupColor", 92, lookup_body, M::READ),
    ];
    cases
}

fn named_color_body(colormap: ResourceId, name: &[u8]) -> Vec<u8> {
    let mut body = colormap.0.to_le_bytes().to_vec();
    body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    body.extend_from_slice(&[0; 2]);
    body.extend_from_slice(name);
    body.resize(body.len().div_ceil(4) * 4, 0);
    body
}

struct ColormapAccessFixture {
    state: ServerState,
    backend: RecordingBackend,
    peer: UnixStream,
}

fn colormap_access_fixture() -> ColormapAccessFixture {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, CLIENT.0);
    state
        .resources
        .create_colormap(CLIENT, COLORMAP, crate::resources::ROOT_VISUAL);
    ColormapAccessFixture {
        state,
        backend: RecordingBackend::new(),
        peer,
    }
}

fn colormap_request(
    fixture: &mut ColormapAccessFixture,
    sequence: u16,
    opcode: u8,
    data: u8,
    body: &[u8],
) -> Vec<u8> {
    process_request(
        &mut fixture.state,
        &mut fixture.backend,
        CLIENT,
        SequenceNumber(sequence),
        RequestHeader {
            opcode,
            data,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("colormap request through process_request");
    read_all_available(&mut fixture.peer)
}

#[test]
fn colormap_resource_requests_deny_each_target_before_effects() {
    // Mutation: remove FreeColormap's resource check; its denial and retained-colormap assertions
    // must fail.
    for case in colormap_access_cases() {
        for (deny_index, (denied_target, denied_mode)) in case.checks.iter().enumerate() {
            let mut fixture = colormap_access_fixture();
            if let Some(installed) = &case.installed_colormaps {
                fixture.state.installed_colormaps.clone_from(installed);
            }
            let calls = Arc::new(Mutex::new(Vec::new()));
            let denied_id = Arc::new(AtomicU32::new(denied_target.id.0));
            fixture
                .state
                .install_access_policy(Arc::new(window_resource_policy(
                    Arc::clone(&calls),
                    Some(Arc::clone(&denied_id)),
                    Some(*denied_mode),
                    false,
                )));

            let colormap_before = fixture
                .state
                .resources
                .colormap(COLORMAP)
                .map(|colormap| format!("{colormap:?}"));
            let new_colormap_before = fixture
                .state
                .resources
                .colormap(NEW_COLORMAP)
                .map(|colormap| format!("{colormap:?}"));
            let installed_before = fixture.state.installed_colormaps.clone();
            let backend_before = fixture.backend.calls();
            let sequence = 200 + u16::try_from(deny_index).unwrap();
            let bytes =
                colormap_request(&mut fixture, sequence, case.opcode, case.data, &case.body);
            assert_x11_error(&bytes, x11::error::BAD_VALUE, 0x55, case.opcode, sequence);
            assert_eq!(
                fixture.backend.calls(),
                backend_before,
                "{} backend",
                case.label
            );
            assert_eq!(
                fixture
                    .state
                    .resources
                    .colormap(COLORMAP)
                    .map(|colormap| format!("{colormap:?}")),
                colormap_before,
                "{} source colormap",
                case.label
            );
            assert_eq!(
                fixture
                    .state
                    .resources
                    .colormap(NEW_COLORMAP)
                    .map(|colormap| format!("{colormap:?}")),
                new_colormap_before,
                "{} new colormap",
                case.label
            );
            assert_eq!(
                fixture.state.installed_colormaps, installed_before,
                "{} installed colormaps",
                case.label
            );

            let recorded = calls.lock().unwrap();
            let expected = &case.checks[..=deny_index];
            assert_eq!(
                recorded.len(),
                expected.len(),
                "{} checked prefix",
                case.label
            );
            for (call, (target, mode)) in recorded.iter().zip(expected) {
                assert_eq!(call.client, CLIENT, "{} client", case.label);
                assert_eq!(
                    call.request,
                    Some(RequestOpcodes {
                        major: case.opcode,
                        minor: 0
                    }),
                    "{} request",
                    case.label
                );
                assert_eq!(call.target, Some(*target), "{} target", case.label);
                assert_eq!(call.mode, Some(*mode), "{} mode", case.label);
            }
        }
    }
}

#[test]
fn colormap_resource_requests_ask_every_expected_target() {
    // Mutation: change QueryColors from READ to WRITE; its recorded-mode assertion must fail.
    for case in colormap_access_cases() {
        let mut fixture = colormap_access_fixture();
        if let Some(installed) = &case.installed_colormaps {
            fixture.state.installed_colormaps.clone_from(installed);
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        fixture
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::clone(&calls),
                None,
                None,
                false,
            )));
        let _ = colormap_request(&mut fixture, 240, case.opcode, case.data, &case.body);
        let recorded = calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            case.checks.len(),
            "{} check count",
            case.label
        );
        for (call, (target, mode)) in recorded.iter().zip(&case.checks) {
            assert_eq!(call.client, CLIENT, "{} client", case.label);
            assert_eq!(
                call.request,
                Some(RequestOpcodes {
                    major: case.opcode,
                    minor: 0
                }),
                "{} request",
                case.label
            );
            assert_eq!(call.target, Some(*target), "{} target", case.label);
            assert_eq!(call.mode, Some(*mode), "{} mode", case.label);
        }
    }
}

#[test]
fn colormap_unconditional_errors_survive_allow_all_policy() {
    // Mutation: skip AllocColorCells' unconditional BadAlloc after Allow; byte equality with the
    // no-policy reply must fail.
    for (opcode, data, body, expected_code) in [
        (
            86,
            0,
            words(&[COLORMAP.0, 1 | (1 << 16)]),
            x11::error::BAD_ALLOC,
        ),
        (
            87,
            0,
            words(&[COLORMAP.0, 1 | (1 << 16), 1 | (1 << 16)]),
            x11::error::BAD_ALLOC,
        ),
        (89, 0, words(&[COLORMAP.0]), x11::error::BAD_ACCESS),
        (
            90,
            0,
            {
                let mut body = COLORMAP.0.to_le_bytes().to_vec();
                body.extend_from_slice(&[0; 4]);
                body.extend_from_slice(&[0; 4]);
                body
            },
            x11::error::BAD_ACCESS,
        ),
    ] {
        let mut baseline = colormap_access_fixture();
        let baseline_bytes = colormap_request(&mut baseline, 250, opcode, data, &body);
        assert_x11_error(&baseline_bytes, expected_code, 0, opcode, 250);

        let mut allowed = colormap_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::new()));
        allowed
            .state
            .install_access_policy(Arc::new(window_resource_policy(
                Arc::clone(&calls),
                None,
                None,
                false,
            )));
        let allowed_bytes = colormap_request(&mut allowed, 250, opcode, data, &body);
        assert_eq!(
            allowed_bytes, baseline_bytes,
            "opcode {opcode} allow-all bytes"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "opcode {opcode} access check"
        );
    }
}
