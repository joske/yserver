use super::*;

const SHAPE_ACCESS_CLIENT: u32 = 1;
const SHAPE_ACCESS_DEST: ResourceId = ResourceId(0x0100_1001);
const SHAPE_ACCESS_SOURCE: ResourceId = ResourceId(0x0100_1002);
const SHAPE_ACCESS_PIXMAP: ResourceId = ResourceId(0x0100_1003);
const SHAPE_ACCESS_ERROR_VALUE: u32 = 0x5a17;

#[derive(Clone, Copy)]
struct ShapeAccessCheck {
    id: ResourceId,
    kind: crate::access::ResourceKind,
    mode: crate::access::AccessMode,
}

struct ShapeAccessCase {
    name: &'static str,
    minor: u8,
    body: Vec<u8>,
    expected: Vec<ShapeAccessCheck>,
}

fn shape_access_fixture(with_resources: bool) -> (ServerState, RecordingBackend, UnixStream) {
    let mut state = ServerState::new();
    let peer = install_client(&mut state, SHAPE_ACCESS_CLIENT);
    if with_resources {
        for id in [SHAPE_ACCESS_DEST, SHAPE_ACCESS_SOURCE] {
            state.resources.create_window(
                ClientId(SHAPE_ACCESS_CLIENT),
                CreateWindowRequest {
                    depth: 24,
                    window: id,
                    parent: ROOT_WINDOW,
                    width: 20,
                    height: 20,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    ..Default::default()
                },
            );
            crate::nested::set_shape_region(
                &mut state,
                id,
                yserver_protocol::x11::shape::KIND_INPUT,
                crate::region::Region::rect(yserver_protocol::x11::xfixes::RegionRect {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                }),
            );
        }
        state.resources.create_pixmap(
            ClientId(SHAPE_ACCESS_CLIENT),
            CreatePixmapRequest {
                depth: 1,
                pixmap: SHAPE_ACCESS_PIXMAP,
                drawable: SHAPE_ACCESS_DEST,
                width: 4,
                height: 4,
            },
        );
    }
    (state, RecordingBackend::new(), peer)
}

fn shape_access_cases(
    dest: ResourceId,
    source: ResourceId,
    pixmap: ResourceId,
) -> Vec<ShapeAccessCase> {
    use crate::access::{AccessMode, ResourceKind};
    use yserver_protocol::x11::shape as x11shape;

    let rect = || {
        let mut body = vec![
            x11shape::OP_SET,
            x11shape::KIND_INPUT,
            x11shape::ORDERING_YX_BANDED,
            0,
        ];
        body.extend_from_slice(&dest.0.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes());
        body.extend_from_slice(&0i16.to_le_bytes());
        body.extend_from_slice(&[0, 0, 2, 0, 2, 0, 2, 0]);
        body
    };
    let mut mask = vec![x11shape::OP_SET, x11shape::KIND_INPUT, 0, 0];
    mask.extend_from_slice(&dest.0.to_le_bytes());
    mask.extend_from_slice(&0i16.to_le_bytes());
    mask.extend_from_slice(&0i16.to_le_bytes());
    mask.extend_from_slice(&pixmap.0.to_le_bytes());
    let mut combine = vec![
        x11shape::OP_SET,
        x11shape::KIND_INPUT,
        x11shape::KIND_INPUT,
        0,
    ];
    combine.extend_from_slice(&dest.0.to_le_bytes());
    combine.extend_from_slice(&0i16.to_le_bytes());
    combine.extend_from_slice(&0i16.to_le_bytes());
    combine.extend_from_slice(&source.0.to_le_bytes());
    let mut offset = vec![x11shape::KIND_INPUT, 0, 0, 0];
    offset.extend_from_slice(&dest.0.to_le_bytes());
    offset.extend_from_slice(&1i16.to_le_bytes());
    offset.extend_from_slice(&1i16.to_le_bytes());
    let mut select = dest.0.to_le_bytes().to_vec();
    select.extend_from_slice(&[1, 0, 0, 0]);
    let mut get_rectangles = dest.0.to_le_bytes().to_vec();
    get_rectangles.extend_from_slice(&[x11shape::KIND_INPUT, 0, 0, 0]);

    vec![
        ShapeAccessCase {
            name: "Rectangles destination",
            minor: x11shape::RECTANGLES,
            body: rect(),
            expected: vec![ShapeAccessCheck {
                id: dest,
                kind: ResourceKind::Window,
                mode: AccessMode::SET_ATTR,
            }],
        },
        ShapeAccessCase {
            name: "Mask destination then source pixmap",
            minor: x11shape::MASK,
            body: mask,
            expected: vec![
                ShapeAccessCheck {
                    id: dest,
                    kind: ResourceKind::Window,
                    mode: AccessMode::SET_ATTR,
                },
                ShapeAccessCheck {
                    id: pixmap,
                    kind: ResourceKind::Pixmap,
                    mode: AccessMode::READ,
                },
            ],
        },
        ShapeAccessCase {
            name: "Combine source then destination",
            minor: x11shape::COMBINE,
            body: combine,
            expected: vec![
                ShapeAccessCheck {
                    id: source,
                    kind: ResourceKind::Window,
                    mode: AccessMode::GET_ATTR,
                },
                ShapeAccessCheck {
                    id: dest,
                    kind: ResourceKind::Window,
                    mode: AccessMode::SET_ATTR,
                },
            ],
        },
        ShapeAccessCase {
            name: "Offset destination",
            minor: x11shape::OFFSET,
            body: offset,
            expected: vec![ShapeAccessCheck {
                id: dest,
                kind: ResourceKind::Window,
                mode: AccessMode::SET_ATTR,
            }],
        },
        ShapeAccessCase {
            name: "QueryExtents window",
            minor: x11shape::QUERY_EXTENTS,
            body: dest.0.to_le_bytes().to_vec(),
            expected: vec![ShapeAccessCheck {
                id: dest,
                kind: ResourceKind::Window,
                mode: AccessMode::GET_ATTR,
            }],
        },
        ShapeAccessCase {
            name: "SelectInput window",
            minor: x11shape::SELECT_INPUT,
            body: select,
            expected: vec![ShapeAccessCheck {
                id: dest,
                kind: ResourceKind::Window,
                mode: AccessMode::RECEIVE,
            }],
        },
        ShapeAccessCase {
            name: "InputSelected window",
            minor: x11shape::INPUT_SELECTED,
            body: dest.0.to_le_bytes().to_vec(),
            expected: vec![ShapeAccessCheck {
                id: dest,
                kind: ResourceKind::Window,
                mode: AccessMode::GET_ATTR,
            }],
        },
        ShapeAccessCase {
            name: "GetRectangles window",
            minor: x11shape::GET_RECTANGLES,
            body: get_rectangles,
            expected: vec![ShapeAccessCheck {
                id: dest,
                kind: ResourceKind::Window,
                mode: AccessMode::GET_ATTR,
            }],
        },
    ]
}

fn shape_access_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    body: &[u8],
    minor: u8,
    sequence: u16,
) {
    process_request(
        state,
        backend,
        ClientId(SHAPE_ACCESS_CLIENT),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: 141,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("SHAPE request");
}

fn shape_resource_checks(
    calls: &[super::extensions::RecordedExtensionCheck],
) -> Vec<(crate::access::ResourceTarget, crate::access::AccessMode)> {
    calls
        .iter()
        .filter(|call| call.hook == super::extensions::ExtensionHook::Resource)
        .map(|call| {
            (
                call.target.expect("resource hook target"),
                call.mode.expect("resource hook mode"),
            )
        })
        .collect()
}

fn shape_assert_policy_error(bytes: &[u8], minor: u8) {
    assert_eq!(bytes.len(), 32, "policy denial is an X error packet");
    assert_eq!(bytes[0], 0, "error packet type");
    assert_eq!(bytes[1], yserver_protocol::x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        SHAPE_ACCESS_ERROR_VALUE
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(minor)
    );
    assert_eq!(bytes[10], 141, "error identifies the SHAPE major opcode");
}

#[test]
fn shape_access_checks_ask_the_expected_targets_modes_and_order() {
    // Mutation: remove the SHAPE::Rectangles destination SET_ATTR check.
    use super::extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::{Decision, RequestOpcodes, ResourceTarget};

    for case in shape_access_cases(SHAPE_ACCESS_DEST, SHAPE_ACCESS_SOURCE, SHAPE_ACCESS_PIXMAP) {
        let (mut state, mut backend, _peer) = shape_access_fixture(true);
        let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        shape_access_request(&mut state, &mut backend, &case.body, case.minor, 1);
        let recorded = calls.lock().unwrap();
        let checks = shape_resource_checks(&recorded);
        let expected = case
            .expected
            .iter()
            .map(|check| {
                (
                    ResourceTarget {
                        owner: None,
                        id: check.id,
                        kind: check.kind,
                        creation: None,
                    },
                    check.mode,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(checks, expected, "{}: resource checks", case.name);
        assert!(recorded.iter().any(|call| {
            call.hook == ExtensionHook::Dispatch
                && call.request
                    == Some(RequestOpcodes {
                        major: 141,
                        minor: case.minor,
                    })
        }));
        for call in recorded
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
        {
            assert_eq!(
                call.client,
                ClientId(SHAPE_ACCESS_CLIENT),
                "{}: client",
                case.name
            );
            assert_eq!(
                call.request,
                Some(RequestOpcodes {
                    major: 141,
                    minor: case.minor,
                }),
                "{}: request context",
                case.name,
            );
            assert_eq!(
                call.owner,
                Some(ClientId(SHAPE_ACCESS_CLIENT)),
                "{}: owner",
                case.name
            );
        }
    }
}

#[test]
fn shape_access_denials_stop_before_state_or_backend_effects() {
    // Mutations: bypass SHAPE::Mask's source pixmap READ check, or insert a SelectInput
    // selection before checking its window; each must fail its denial assertion.
    use super::extensions::{RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::DenyError;

    for case in shape_access_cases(SHAPE_ACCESS_DEST, SHAPE_ACCESS_SOURCE, SHAPE_ACCESS_PIXMAP) {
        for (denied_index, denied) in case.expected.iter().enumerate() {
            let (mut state, mut backend, mut peer) = shape_access_fixture(true);
            let previous_shapes = state.shape_windows.clone();
            let previous_select_masks = state.shape_select_masks.clone();
            let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
            state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                denied.id,
                Some(denied.mode),
                DenyError {
                    code: yserver_protocol::x11::error::BAD_VALUE,
                    value: SHAPE_ACCESS_ERROR_VALUE,
                },
            )));

            shape_access_request(&mut state, &mut backend, &case.body, case.minor, 1);
            let bytes = read_all_available(&mut peer);
            shape_assert_policy_error(&bytes, case.minor);
            assert_eq!(
                state.shape_windows, previous_shapes,
                "{}: state changed",
                case.name
            );
            assert_eq!(
                state.shape_select_masks, previous_select_masks,
                "{}: SHAPE selection state changed",
                case.name
            );
            assert!(
                backend.calls().is_empty(),
                "{}: backend was reached",
                case.name
            );
            let recorded = calls.lock().unwrap();
            let checks = shape_resource_checks(&recorded);
            let expected = case.expected[..=denied_index]
                .iter()
                .map(|check| {
                    (
                        crate::access::ResourceTarget {
                            owner: None,
                            id: check.id,
                            kind: check.kind,
                            creation: None,
                        },
                        check.mode,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(checks, expected, "{}: checks before denial", case.name);
        }
    }
}

#[test]
fn shape_unresolved_operands_keep_no_policy_wire_bytes() {
    // Mutation: reject an unresolved SHAPE operand on the empty-policy fast path.
    use yserver_protocol::x11::{ClientByteOrder, shape as x11shape, xfixes::RegionRect};

    const MISSING: ResourceId = ResourceId(0x7f00_1234);
    const MISSING_SOURCE: ResourceId = ResourceId(0x7f00_1235);
    for case in shape_access_cases(MISSING, MISSING_SOURCE, MISSING_SOURCE) {
        let (mut state, mut backend, mut peer) = shape_access_fixture(false);
        shape_access_request(&mut state, &mut backend, &case.body, case.minor, 1);
        let expected_bytes = match case.minor {
            x11shape::QUERY_EXTENTS => x11shape::encode_query_extents_reply(
                ClientByteOrder::LittleEndian,
                SequenceNumber(1),
                false,
                false,
                RegionRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                RegionRect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                },
            ),
            x11shape::INPUT_SELECTED => x11shape::encode_input_selected_reply(
                ClientByteOrder::LittleEndian,
                SequenceNumber(1),
                false,
            ),
            x11shape::GET_RECTANGLES => x11shape::encode_get_rectangles_reply(
                ClientByteOrder::LittleEndian,
                SequenceNumber(1),
                x11shape::ORDERING_YX_BANDED,
                &[],
            ),
            _ => Vec::new(),
        };
        assert_eq!(
            read_all_available(&mut peer),
            expected_bytes,
            "{}: bytes without policy",
            case.name,
        );
        assert!(
            state
                .clients
                .get(&SHAPE_ACCESS_CLIENT)
                .unwrap()
                .outbound
                .is_empty(),
            "{}: no buffered extra bytes",
            case.name,
        );
        if case.minor == x11shape::SELECT_INPUT {
            assert_eq!(
                state
                    .shape_select_masks
                    .get(&(SHAPE_ACCESS_CLIENT, MISSING)),
                Some(&true),
                "SelectInput's unresolved window keeps its historical selection entry",
            );
        }
    }
}

/// XFixes `SetCursorName(cursor, "name")` interns the name as an
/// atom and stores it on the cursor; the subsequent
/// `GetCursorName(cursor)` returns the same atom + name bytes.
/// A cursor that was never named reports atom=0 (None) + empty
/// name, matching Xorg `xfixes/cursor.c:ProcXFixesGetCursorName`'s
/// `pCursor->name == 0` branch.
#[test]
fn xfixes_cursor_name_round_trip() {
    use std::io::Read;

    const CLIENT_ID: u32 = 1;
    const CURSOR_XID: u32 = 0x0090_0042;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, CLIENT_ID);
    let mut backend = RecordingBackend::new();
    state
        .resources
        .create_cursor(ClientId(CLIENT_ID), ResourceId(CURSOR_XID));

    let name = b"xterm";
    // SetCursorName body: cursor(4) + nbytes(2) + pad(2) + name + pad.
    let mut set_body = Vec::with_capacity(16);
    set_body.extend_from_slice(&CURSOR_XID.to_le_bytes());
    #[allow(clippy::cast_possible_truncation)]
    set_body.extend_from_slice(&(name.len() as u16).to_le_bytes());
    set_body.extend_from_slice(&[0u8; 2]);
    set_body.extend_from_slice(name);
    while !set_body.len().is_multiple_of(4) {
        set_body.push(0);
    }

    let header = yserver_protocol::x11::RequestHeader {
        opcode: 140, // XFIXES major (dispatcher reads minor from header.data)
        data: 23,    // SET_CURSOR_NAME
        length_units: 3,
    };
    handle_xfixes_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(1),
        header,
        &set_body,
    )
    .expect("SetCursorName");

    // The atom must be stored on the cursor and resolve back to
    // the original name.
    let stored = state
        .resources
        .cursor_name_atom(ResourceId(CURSOR_XID))
        .expect("name atom present after SetCursorName");
    assert!(stored.0 != 0, "non-None atom expected for non-empty name");
    assert_eq!(
        state.atoms.name(stored).map(str::as_bytes),
        Some(name.as_ref()),
        "interned atom must reverse-resolve to the original name",
    );

    // GetCursorName body: cursor(4).
    let get_body = CURSOR_XID.to_le_bytes().to_vec();
    let get_header = yserver_protocol::x11::RequestHeader {
        opcode: 140,
        data: 24, // GET_CURSOR_NAME
        length_units: 2,
    };
    handle_xfixes_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(2),
        get_header,
        &get_body,
    )
    .expect("GetCursorName");

    peer.set_nonblocking(true).unwrap();
    let mut wire = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // Reply layout: type(1) + pad(1) + sequence(2) + length(4) +
    // atom(4) + nbytes(2) + pad(18) + name(n) + pad-to-4.
    assert_eq!(wire[0], 1, "X_Reply type byte");
    let reply_atom = u32::from_le_bytes(wire[8..12].try_into().unwrap());
    let reply_nbytes = u16::from_le_bytes(wire[12..14].try_into().unwrap()) as usize;
    assert_eq!(reply_atom, stored.0, "reply atom matches stored atom");
    assert_eq!(reply_nbytes, name.len());
    assert_eq!(
        &wire[32..32 + name.len()],
        name,
        "reply name bytes match the original",
    );

    // GetCursorName on an unnamed cursor → atom=0, nbytes=0.
    const UNNAMED_XID: u32 = 0x0090_0099;
    state
        .resources
        .create_cursor(ClientId(CLIENT_ID), ResourceId(UNNAMED_XID));
    let get_body = UNNAMED_XID.to_le_bytes().to_vec();
    handle_xfixes_request(
        &mut state,
        &mut backend,
        None,
        ClientId(CLIENT_ID),
        SequenceNumber(3),
        get_header,
        &get_body,
    )
    .expect("GetCursorName unnamed");
    let mut wire2 = Vec::new();
    loop {
        match peer.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => wire2.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    let reply_atom = u32::from_le_bytes(wire2[8..12].try_into().unwrap());
    let reply_nbytes = u16::from_le_bytes(wire2[12..14].try_into().unwrap()) as usize;
    assert_eq!(reply_atom, 0, "unnamed cursor reports atom=0 (None)");
    assert_eq!(reply_nbytes, 0, "unnamed cursor reports empty name");
}

/// One XFIXES request through the real dispatcher (version gate
/// included), little-endian body.
fn xfixes_req(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    minor: u8,
    body: &[u8],
) {
    process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("xfixes request");
}

fn xfixes_u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

const XFIXES_OWNER: u32 = 1;
const XFIXES_WRITER: u32 = 2;
const XFIXES_REGION: u32 = 0x0100_0001;
const XFIXES_SOURCE: u32 = 0x0100_0002;
const XFIXES_SOURCE2: u32 = 0x0100_0003;

const XFIXES_ACCESS_WINDOW: ResourceId = ResourceId(0x0100_0010);
const XFIXES_ACCESS_PIXMAP: ResourceId = ResourceId(0x0100_0011);
const XFIXES_ACCESS_GC: ResourceId = ResourceId(0x0100_0012);
const XFIXES_ACCESS_PICTURE: ResourceId = ResourceId(0x0100_0013);
const XFIXES_ACCESS_REGION: ResourceId = ResourceId(0x0100_0004);
const XFIXES_ACCESS_REGION2: ResourceId = ResourceId(0x0100_0005);
const XFIXES_ACCESS_REGION3: ResourceId = ResourceId(0x0100_0006);
const XFIXES_ACCESS_NEW_REGION: ResourceId = ResourceId(0x0200_0001);
const XFIXES_ACCESS_ERROR_VALUE: u32 = 0x4f21;

#[derive(Clone, Copy)]
struct XfixesAccessCheck {
    target: crate::access::ResourceTarget,
    mode: crate::access::AccessMode,
}

struct XfixesAccessCase {
    name: &'static str,
    major: u8,
    minor: u8,
    body: Vec<u8>,
    expected: Vec<XfixesAccessCheck>,
}

fn xfixes_existing_check(
    id: ResourceId,
    kind: crate::access::ResourceKind,
    mode: crate::access::AccessMode,
) -> XfixesAccessCheck {
    XfixesAccessCheck {
        target: crate::access::ResourceTarget {
            owner: None,
            id,
            kind,
            creation: None,
        },
        mode,
    }
}

fn xfixes_creation_check(id: ResourceId, parent: Option<ResourceId>) -> XfixesAccessCheck {
    XfixesAccessCheck {
        target: crate::access::ResourceTarget {
            owner: None,
            id,
            kind: crate::access::ResourceKind::Region,
            creation: Some(crate::access::Creation {
                parent,
                owner: ClientId(XFIXES_WRITER),
            }),
        },
        mode: crate::access::AccessMode::CREATE,
    }
}

fn xfixes_region_access_fixture() -> (ServerState, RecordingBackend, UnixStream) {
    use crate::{
        backend::PictureHandle,
        resources::{PictureKind, PictureState},
    };

    let mut state = ServerState::new();
    let _owner_peer = install_client(&mut state, XFIXES_OWNER);
    let writer_peer = install_client(&mut state, XFIXES_WRITER);
    state
        .xfixes_client_major
        .insert(XFIXES_WRITER, yserver_protocol::x11::xfixes::MAJOR_VERSION);
    state.resources.create_window(
        ClientId(XFIXES_OWNER),
        CreateWindowRequest {
            depth: 24,
            window: XFIXES_ACCESS_WINDOW,
            parent: crate::resources::ROOT_WINDOW,
            width: 20,
            height: 20,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_pixmap(
        ClientId(XFIXES_OWNER),
        CreatePixmapRequest {
            depth: 24,
            pixmap: XFIXES_ACCESS_PIXMAP,
            drawable: XFIXES_ACCESS_WINDOW,
            width: 20,
            height: 20,
        },
    );
    state.resources.create_gc(
        ClientId(XFIXES_OWNER),
        CreateGcRequest {
            gc: XFIXES_ACCESS_GC,
            drawable: XFIXES_ACCESS_WINDOW,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );
    state.resources.set_clip_rectangles(
        ClientId(XFIXES_OWNER),
        yserver_protocol::x11::SetClipRectanglesRequest {
            gc: XFIXES_ACCESS_GC,
            clip: yserver_protocol::x11::ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: [
                    0i16.to_le_bytes(),
                    0i16.to_le_bytes(),
                    4u16.to_le_bytes(),
                    4u16.to_le_bytes(),
                ]
                .concat(),
            },
        },
    );
    state.resources.create_picture(
        XFIXES_ACCESS_PICTURE,
        PictureState {
            client: ClientId(XFIXES_OWNER),
            host_picture_xid: Some(PictureHandle::from_raw_for_test(0x4413)),
            host_owned_pixmap: None,
            kind: PictureKind::Drawable,
            drawable: Some(XFIXES_ACCESS_WINDOW),
            window: None,
        },
    );
    let rects = [
        yserver_protocol::x11::xfixes::RegionRect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        },
        yserver_protocol::x11::xfixes::RegionRect {
            x: 10,
            y: 2,
            width: 3,
            height: 4,
        },
        yserver_protocol::x11::xfixes::RegionRect {
            x: 1,
            y: 12,
            width: 3,
            height: 4,
        },
    ];
    for (id, rect) in [
        XFIXES_ACCESS_REGION,
        XFIXES_ACCESS_REGION2,
        XFIXES_ACCESS_REGION3,
    ]
    .into_iter()
    .zip(rects)
    {
        state.xfixes_regions.insert(
            id.0,
            crate::server::XFixesRegion {
                owner: ClientId(XFIXES_OWNER),
                region: crate::region::Region::rect(rect),
            },
        );
    }
    let mut backend = RecordingBackend::new();
    backend.picture_client_clip_rects_result = Some(Some(vec![rects[0]]));
    (state, backend, writer_peer)
}

fn xfixes_region_access_cases() -> Vec<XfixesAccessCase> {
    use crate::access::{AccessMode as M, ResourceKind as K};
    use yserver_protocol::x11::{composite as c, xfixes as x};

    let words = |values: &[u32]| {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    };
    let rect_body = || {
        xfixes_rect_body(
            XFIXES_ACCESS_REGION.0,
            yserver_protocol::x11::xfixes::RegionRect {
                x: 4,
                y: 5,
                width: 6,
                height: 7,
            },
        )
    };
    let new = XFIXES_ACCESS_NEW_REGION;
    let region = XFIXES_ACCESS_REGION;
    let region2 = XFIXES_ACCESS_REGION2;
    let region3 = XFIXES_ACCESS_REGION3;
    let window = XFIXES_ACCESS_WINDOW;
    let pixmap = XFIXES_ACCESS_PIXMAP;
    let gc = XFIXES_ACCESS_GC;
    let picture = XFIXES_ACCESS_PICTURE;
    let mut cases = Vec::new();
    let mut add = |name, major, minor, body, expected| {
        cases.push(XfixesAccessCase {
            name,
            major,
            minor,
            body,
            expected,
        });
    };

    let create_region_body = xfixes_rect_body(
        new.0,
        yserver_protocol::x11::xfixes::RegionRect {
            x: 4,
            y: 5,
            width: 6,
            height: 7,
        },
    );
    add(
        "CreateRegion",
        140,
        x::CREATE_REGION,
        create_region_body,
        vec![xfixes_creation_check(new, None)],
    );
    add(
        "CreateRegionFromBitmap",
        140,
        x::CREATE_REGION_FROM_BITMAP,
        words(&[new.0, pixmap.0]),
        vec![
            xfixes_existing_check(pixmap, K::Pixmap, M::READ),
            xfixes_creation_check(new, Some(pixmap)),
        ],
    );
    let mut from_window = words(&[new.0, window.0]);
    from_window.extend_from_slice(&[0, 0, 0, 0]);
    add(
        "CreateRegionFromWindow",
        140,
        x::CREATE_REGION_FROM_WINDOW,
        from_window,
        vec![
            xfixes_existing_check(window, K::Window, M::GET_ATTR),
            xfixes_creation_check(new, Some(window)),
        ],
    );
    add(
        "CreateRegionFromGC",
        140,
        x::CREATE_REGION_FROM_GC,
        words(&[new.0, gc.0]),
        vec![
            xfixes_existing_check(gc, K::Gc, M::GET_ATTR),
            xfixes_creation_check(new, None),
        ],
    );
    add(
        "CreateRegionFromPicture",
        140,
        x::CREATE_REGION_FROM_PICTURE,
        words(&[new.0, picture.0]),
        vec![
            xfixes_existing_check(picture, K::Picture, M::GET_ATTR),
            xfixes_creation_check(new, None),
        ],
    );
    add(
        "DestroyRegion",
        140,
        x::DESTROY_REGION,
        words(&[region.0, 0]),
        vec![xfixes_existing_check(region, K::Region, M::WRITE)],
    );
    add(
        "SetRegion",
        140,
        x::SET_REGION,
        rect_body(),
        vec![xfixes_existing_check(region, K::Region, M::WRITE)],
    );
    add(
        "CopyRegion",
        140,
        x::COPY_REGION,
        words(&[region.0, region2.0]),
        vec![
            xfixes_existing_check(region, K::Region, M::READ),
            xfixes_existing_check(region2, K::Region, M::WRITE),
        ],
    );
    for minor in [x::UNION_REGION, x::INTERSECT_REGION, x::SUBTRACT_REGION] {
        add(
            "two-source region op",
            140,
            minor,
            words(&[region.0, region2.0, region3.0]),
            vec![
                xfixes_existing_check(region, K::Region, M::READ),
                xfixes_existing_check(region2, K::Region, M::READ),
                xfixes_existing_check(region3, K::Region, M::WRITE),
            ],
        );
    }
    let mut invert = words(&[region.0]);
    invert.extend_from_slice(
        &[
            0i16.to_le_bytes(),
            0i16.to_le_bytes(),
            20u16.to_le_bytes(),
            20u16.to_le_bytes(),
        ]
        .concat(),
    );
    invert.extend_from_slice(&region2.0.to_le_bytes());
    add(
        "InvertRegion",
        140,
        x::INVERT_REGION,
        invert,
        vec![
            xfixes_existing_check(region, K::Region, M::READ),
            xfixes_existing_check(region2, K::Region, M::WRITE),
        ],
    );
    let mut translate = region.0.to_le_bytes().to_vec();
    translate.extend_from_slice(&[1i16.to_le_bytes(), 2i16.to_le_bytes()].concat());
    add(
        "TranslateRegion",
        140,
        x::TRANSLATE_REGION,
        translate,
        vec![xfixes_existing_check(region, K::Region, M::WRITE)],
    );
    add(
        "RegionExtents",
        140,
        x::REGION_EXTENTS,
        words(&[region.0, region2.0]),
        vec![
            xfixes_existing_check(region, K::Region, M::READ),
            xfixes_existing_check(region2, K::Region, M::WRITE),
        ],
    );
    add(
        "FetchRegion",
        140,
        x::FETCH_REGION,
        words(&[region.0]),
        vec![xfixes_existing_check(region, K::Region, M::READ)],
    );
    let mut set_gc_clip = words(&[gc.0, region.0]);
    set_gc_clip.extend_from_slice(&[0i16.to_le_bytes(), 0i16.to_le_bytes()].concat());
    add(
        "SetGCClipRegion",
        140,
        x::SET_GC_CLIP_REGION,
        set_gc_clip,
        vec![
            xfixes_existing_check(gc, K::Gc, M::SET_ATTR),
            xfixes_existing_check(region, K::Region, M::READ),
        ],
    );
    let mut set_shape = words(&[window.0]);
    set_shape.extend_from_slice(&[0, 0, 0, 0]);
    set_shape.extend_from_slice(&[0i16.to_le_bytes(), 0i16.to_le_bytes()].concat());
    set_shape.extend_from_slice(&region.0.to_le_bytes());
    add(
        "SetWindowShapeRegion",
        140,
        x::SET_WINDOW_SHAPE_REGION,
        set_shape,
        vec![
            xfixes_existing_check(window, K::Window, M::SET_ATTR),
            xfixes_existing_check(region, K::Region, M::WRITE),
        ],
    );
    let mut set_picture_clip = words(&[picture.0, region.0]);
    set_picture_clip.extend_from_slice(&[0i16.to_le_bytes(), 0i16.to_le_bytes()].concat());
    add(
        "SetPictureClipRegion",
        140,
        x::SET_PICTURE_CLIP_REGION,
        set_picture_clip,
        vec![
            xfixes_existing_check(picture, K::Picture, M::SET_ATTR),
            xfixes_existing_check(region, K::Region, M::READ),
        ],
    );
    let mut expand = words(&[region.0, region2.0]);
    expand.extend_from_slice(
        &[
            1u16.to_le_bytes(),
            2u16.to_le_bytes(),
            3u16.to_le_bytes(),
            4u16.to_le_bytes(),
        ]
        .concat(),
    );
    add(
        "ExpandRegion",
        140,
        x::EXPAND_REGION,
        expand,
        vec![
            xfixes_existing_check(region, K::Region, M::READ),
            xfixes_existing_check(region2, K::Region, M::WRITE),
        ],
    );
    add(
        "CreateRegionFromBorderClip",
        144,
        c::CREATE_REGION_FROM_BORDER_CLIP,
        words(&[new.0, window.0]),
        vec![
            xfixes_existing_check(window, K::Window, M::GET_ATTR),
            xfixes_creation_check(new, Some(window)),
        ],
    );
    cases
}

fn xfixes_region_access_request(
    state: &mut ServerState,
    backend: &mut RecordingBackend,
    client: u32,
    major: u8,
    minor: u8,
    body: &[u8],
    sequence: u16,
) {
    process_request(
        state,
        backend,
        ClientId(client),
        SequenceNumber(sequence),
        RequestHeader {
            opcode: major,
            data: minor,
            length_units: u32::try_from(1 + body.len().div_ceil(4)).unwrap(),
        },
        body,
        None,
    )
    .expect("region access request");
}

#[test]
fn xfixes_region_access_checks_ask_expected_targets_modes_and_order() {
    // Mutation: remove CopyRegion's source READ check; the recorded operand order must fail.
    use super::extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::{Decision, RequestOpcodes};

    for case in xfixes_region_access_cases() {
        let (mut state, mut backend, _peer) = xfixes_region_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::new(
            Decision::Allow,
            Arc::clone(&calls),
        )));
        xfixes_region_access_request(
            &mut state,
            &mut backend,
            XFIXES_WRITER,
            case.major,
            case.minor,
            &case.body,
            1,
        );
        let recorded = calls.lock().unwrap();
        let actual = recorded
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .map(|call| {
                (
                    call.target.expect("resource target"),
                    call.mode.expect("resource mode"),
                )
            })
            .collect::<Vec<_>>();
        let expected = case
            .expected
            .iter()
            .map(|check| (check.target, check.mode))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{}: resource checks", case.name);
        for call in recorded
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
        {
            assert_eq!(
                call.client,
                ClientId(XFIXES_WRITER),
                "{}: client",
                case.name
            );
            assert_eq!(
                call.request,
                Some(RequestOpcodes {
                    major: case.major,
                    minor: case.minor,
                }),
                "{}: request opcodes",
                case.name,
            );
            let expected_owner = call
                .target
                .and_then(|target| target.creation.is_none().then_some(ClientId(XFIXES_OWNER)));
            assert_eq!(call.owner, expected_owner, "{}: target owner", case.name);
            assert_eq!(
                call.target_owner, expected_owner,
                "{}: target instance owner",
                case.name
            );
        }
    }
}

#[test]
fn xfixes_region_access_denials_stop_before_state_or_backend_effects() {
    // Mutation: move CopyRegion's and UnionRegion's destination WRITE checks after their stores; retained-state assertions must fail.
    use super::extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::DenyError;

    for case in xfixes_region_access_cases() {
        for (denied_index, denied) in case.expected.iter().enumerate() {
            let (mut state, mut backend, mut peer) = xfixes_region_access_fixture();
            let regions_before = state
                .xfixes_regions
                .iter()
                .map(|(id, region)| (*id, region.owner, region.region.to_rects()))
                .collect::<Vec<_>>();
            let shapes_before = state.shape_windows.clone();
            let gc_clip_before = state
                .resources
                .gc(XFIXES_ACCESS_GC)
                .and_then(|gc| gc.clip_rectangles.clone());
            let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
            state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
                Arc::clone(&calls),
                denied.target.id,
                Some(denied.mode),
                DenyError {
                    code: yserver_protocol::x11::error::BAD_VALUE,
                    value: XFIXES_ACCESS_ERROR_VALUE,
                },
            )));
            xfixes_region_access_request(
                &mut state,
                &mut backend,
                XFIXES_WRITER,
                case.major,
                case.minor,
                &case.body,
                1,
            );
            let bytes = read_all_available(&mut peer);
            assert_eq!(bytes.len(), 32, "{}: policy denial packet", case.name);
            assert_eq!(bytes[0], 0, "{}: packet type", case.name);
            assert_eq!(
                bytes[1],
                yserver_protocol::x11::error::BAD_VALUE,
                "{}: policy error code",
                case.name
            );
            assert_eq!(
                u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                XFIXES_ACCESS_ERROR_VALUE,
                "{}: policy error value",
                case.name
            );
            assert_eq!(
                u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
                u16::from(case.minor),
                "{}: error minor opcode",
                case.name
            );
            assert_eq!(bytes[10], case.major, "{}: error major opcode", case.name);
            assert_eq!(
                state
                    .xfixes_regions
                    .iter()
                    .map(|(id, region)| (*id, region.owner, region.region.to_rects()))
                    .collect::<Vec<_>>(),
                regions_before,
                "{}: region state changed",
                case.name,
            );
            assert_eq!(
                state.shape_windows, shapes_before,
                "{}: shape state changed",
                case.name
            );
            assert_eq!(
                state
                    .resources
                    .gc(XFIXES_ACCESS_GC)
                    .and_then(|gc| gc.clip_rectangles.clone()),
                gc_clip_before,
                "{}: GC clip changed",
                case.name,
            );
            assert!(
                backend.calls().is_empty(),
                "{}: backend was reached",
                case.name
            );
            let expected = case.expected[..=denied_index]
                .iter()
                .map(|check| (check.target, check.mode))
                .collect::<Vec<_>>();
            let actual = calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.hook == ExtensionHook::Resource)
                .map(|call| {
                    (
                        call.target.expect("resource target"),
                        call.mode.expect("resource mode"),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{}: checks before denial", case.name);
        }
    }
}

#[test]
fn expand_region_source_denial_precedes_missing_destination() {
    // Mutation: validate the destination before checking the source; the policy row must then
    // return BadAccess instead of the later BadRegion.
    use super::extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::{AccessMode, DenyError, RequestOpcodes, ResourceKind};
    use yserver_protocol::x11::xfixes as x;

    let rows = [(x::EXPAND_REGION, XFIXES_ACCESS_REGION, 0xDEAD_4401)];
    for (minor, source, missing) in rows {
        let label = "ExpandRegion";
        let mut body = xfixes_u32s(&[source.0, missing]);
        for margin in [1u16, 2, 3, 4] {
            body.extend_from_slice(&margin.to_le_bytes());
        }
        let snapshot = |state: &ServerState| {
            state
                .xfixes_regions
                .iter()
                .map(|(id, region)| (*id, region.owner, region.region.to_rects()))
                .collect::<Vec<_>>()
        };

        let (mut state, mut backend, mut peer) = xfixes_region_access_fixture();
        let before = snapshot(&state);
        let backend_before = backend.calls();
        xfixes_region_access_request(
            &mut state,
            &mut backend,
            XFIXES_WRITER,
            140,
            minor,
            &body,
            1,
        );
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32, "{label}: expected BadRegion");
        assert_eq!(
            bytes[1],
            crate::nested::XFIXES_FIRST_ERROR,
            "{label}: no-policy error"
        );
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), missing);
        assert_eq!(
            u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
            u16::from(minor)
        );
        assert_eq!(bytes[10], 140);
        assert_eq!(snapshot(&state), before, "{label}: no-policy state changed");
        assert_eq!(
            backend.calls(),
            backend_before,
            "{label}: no-policy backend call"
        );

        let (mut state, mut backend, mut peer) = xfixes_region_access_fixture();
        let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
        let error = DenyError {
            code: x11::error::BAD_ACCESS,
            value: XFIXES_ACCESS_ERROR_VALUE,
        };
        state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
            Arc::clone(&calls),
            source,
            Some(AccessMode::READ),
            error,
        )));
        let before = snapshot(&state);
        let backend_before = backend.calls();
        xfixes_region_access_request(
            &mut state,
            &mut backend,
            XFIXES_WRITER,
            140,
            minor,
            &body,
            1,
        );
        let bytes = read_all_available(&mut peer);
        assert_eq!(bytes.len(), 32, "{label}: expected policy denial");
        assert_eq!(bytes[1], error.code);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            error.value
        );
        assert_eq!(
            u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
            u16::from(minor)
        );
        assert_eq!(bytes[10], 140);
        assert_eq!(snapshot(&state), before, "{label}: denied state changed");
        assert_eq!(
            backend.calls(),
            backend_before,
            "{label}: denied request reached backend"
        );
        let calls = calls.lock().unwrap();
        let resource_calls: Vec<_> = calls
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .collect();
        assert_eq!(
            resource_calls.len(),
            1,
            "{label}: later destination was checked"
        );
        let check = resource_calls[0];
        let target = xfixes_existing_check(source, ResourceKind::Region, AccessMode::READ).target;
        assert_eq!(check.client, ClientId(XFIXES_WRITER));
        assert_eq!(check.target, Some(target));
        assert_eq!(check.mode, Some(AccessMode::READ));
        assert_eq!(check.owner, Some(ClientId(XFIXES_OWNER)));
        assert_eq!(check.target_owner, Some(ClientId(XFIXES_OWNER)));
        assert_eq!(check.request, Some(RequestOpcodes { major: 140, minor }));
    }
}

#[test]
fn region_write_into_other_clients_region_denied_keeps_it() {
    // Mutation: move CopyRegion's destination WRITE check after its store; the rectangle assertion must fail.
    use super::extensions::{RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::{AccessMode, DenyError};
    use yserver_protocol::x11::xfixes as x;

    let (mut state, mut backend, mut writer_peer) = xfixes_region_access_fixture();
    let source = XFIXES_ACCESS_REGION2;
    let destination = XFIXES_ACCESS_REGION;
    let before = state.xfixes_regions[&destination.0].region.to_rects();
    let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        destination,
        Some(AccessMode::WRITE),
        DenyError {
            code: yserver_protocol::x11::error::BAD_VALUE,
            value: XFIXES_ACCESS_ERROR_VALUE,
        },
    )));
    xfixes_region_access_request(
        &mut state,
        &mut backend,
        XFIXES_WRITER,
        140,
        x::COPY_REGION,
        &xfixes_u32s(&[source.0, destination.0]),
        1,
    );
    let bytes = read_all_available(&mut writer_peer);
    assert_eq!(bytes.len(), 32, "policy denial returns one error packet");
    assert_eq!(bytes[0], 0, "error packet type");
    assert_eq!(bytes[1], yserver_protocol::x11::error::BAD_VALUE);
    assert_eq!(
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        XFIXES_ACCESS_ERROR_VALUE
    );
    assert_eq!(
        u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        u16::from(x::COPY_REGION)
    );
    assert_eq!(bytes[10], 140);
    let after = &state.xfixes_regions[&destination.0];
    assert_eq!(
        after.owner,
        ClientId(XFIXES_OWNER),
        "destination owner is retained"
    );
    assert_eq!(
        after.region.to_rects(),
        before,
        "destination rectangles are retained"
    );
    let checked = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| call.hook == super::extensions::ExtensionHook::Resource)
        .filter_map(|call| call.target.map(|target| (target.id, call.mode.unwrap())))
        .collect::<Vec<_>>();
    assert_eq!(
        checked,
        vec![(source, AccessMode::READ), (destination, AccessMode::WRITE),],
        "CopyRegion checks source then destination before denial",
    );
}

#[test]
fn missing_destination_region_is_not_asked() {
    // Mutation: ask the policy about unresolved CopyRegion destinations; this deny-all policy must then fail the request.
    use super::extensions::{ExtensionHook, RecordedExtensionCheck, RecordingExtensionPolicy};
    use crate::access::DenyError;
    use yserver_protocol::x11::xfixes as x;

    const MISSING_SOURCE: ResourceId = ResourceId(0x7f00_1122);
    const MISSING_DESTINATION: ResourceId = ResourceId(0x7f00_3344);
    let (mut state, mut backend, mut writer_peer) = xfixes_region_access_fixture();
    let calls = Arc::new(Mutex::new(Vec::<RecordedExtensionCheck>::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_all_resources(
        Arc::clone(&calls),
        DenyError {
            code: yserver_protocol::x11::error::BAD_VALUE,
            value: XFIXES_ACCESS_ERROR_VALUE,
        },
    )));
    xfixes_region_access_request(
        &mut state,
        &mut backend,
        XFIXES_WRITER,
        140,
        x::COPY_REGION,
        &xfixes_u32s(&[MISSING_SOURCE.0, MISSING_DESTINATION.0]),
        1,
    );
    assert!(
        read_all_available(&mut writer_peer).is_empty(),
        "no-policy wire outcome is unchanged"
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.hook == ExtensionHook::Resource)
            .count(),
        0,
        "unresolved source and destination are not checked",
    );
    let created = state
        .xfixes_regions
        .get(&MISSING_DESTINATION.0)
        .expect("CopyRegion creates the unresolved destination");
    assert_eq!(created.owner, ClientId(XFIXES_WRITER));
    assert!(created.region.to_rects().is_empty());
}

fn xfixes_rect_body(id: u32, rect: yserver_protocol::x11::xfixes::RegionRect) -> Vec<u8> {
    let mut body = id.to_le_bytes().to_vec();
    body.extend_from_slice(&rect.x.to_le_bytes());
    body.extend_from_slice(&rect.y.to_le_bytes());
    body.extend_from_slice(&rect.width.to_le_bytes());
    body.extend_from_slice(&rect.height.to_le_bytes());
    body
}

fn assert_xfixes_single_write_preserves_owner(
    minor: u8,
    body: &[u8],
    expected: &[(i16, i16, u16, u16)],
) {
    use yserver_protocol::x11::xfixes as x11xfixes;

    let mut state = ServerState::new();
    let mut owner_peer = install_client(&mut state, XFIXES_OWNER);
    let mut writer_peer = install_client(&mut state, XFIXES_WRITER);
    state.xfixes_client_major.insert(XFIXES_OWNER, 5);
    state.xfixes_client_major.insert(XFIXES_WRITER, 5);
    let mut backend = RecordingBackend::new();

    let rect = |x, y, width, height| yserver_protocol::x11::xfixes::RegionRect {
        x,
        y,
        width,
        height,
    };
    for (id, initial) in [
        (XFIXES_REGION, rect(1, 2, 3, 4)),
        (XFIXES_SOURCE, rect(10, 20, 30, 40)),
        (XFIXES_SOURCE2, rect(12, 22, 6, 8)),
    ] {
        xfixes_req(
            &mut state,
            &mut backend,
            XFIXES_OWNER,
            x11xfixes::CREATE_REGION,
            &xfixes_rect_body(id, initial),
        );
    }

    // Each invocation starts from fresh owner-created regions and sends one
    // destination-writing request from the other client.
    xfixes_req(&mut state, &mut backend, XFIXES_WRITER, minor, body);
    assert!(
        wire_packets(&mut writer_peer).is_empty(),
        "writer request succeeds"
    );

    crate::core_loop::process_disconnect::process_disconnect(
        &mut state,
        &mut backend,
        ClientId(XFIXES_WRITER),
    );

    xfixes_req(
        &mut state,
        &mut backend,
        XFIXES_OWNER,
        x11xfixes::FETCH_REGION,
        &XFIXES_REGION.to_le_bytes(),
    );
    let reply = read_all_available(&mut owner_peer);
    let expected_wire: Vec<u8> = expected
        .iter()
        .flat_map(|(x, y, width, height)| {
            x.to_le_bytes()
                .into_iter()
                .chain(y.to_le_bytes())
                .chain(width.to_le_bytes())
                .chain(height.to_le_bytes())
        })
        .collect();
    assert_eq!(
        reply.len(),
        32 + expected_wire.len(),
        "FetchRegion returns every expected rectangle after writer disconnect",
    );
    assert_eq!(reply.first(), Some(&1), "FetchRegion returns a reply");
    assert_eq!(
        &reply[32..],
        expected_wire,
        "FetchRegion returns the written region contents",
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; CopyRegion's rectangle must still be fetchable after disconnect.
#[test]
fn xfixes_write_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    assert_xfixes_single_write_preserves_owner(
        x11xfixes::COPY_REGION,
        &xfixes_u32s(&[XFIXES_SOURCE, XFIXES_REGION]),
        &[(10, 20, 30, 40)],
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; SetRegion's replacement must survive that writer's disconnect.
#[test]
fn xfixes_set_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    assert_xfixes_single_write_preserves_owner(
        x11xfixes::SET_REGION,
        &xfixes_rect_body(
            XFIXES_REGION,
            yserver_protocol::x11::xfixes::RegionRect {
                x: 2,
                y: 3,
                width: 4,
                height: 5,
            },
        ),
        &[(2, 3, 4, 5)],
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; UnionRegion's result must survive that writer's disconnect.
#[test]
fn xfixes_union_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    assert_xfixes_single_write_preserves_owner(
        x11xfixes::UNION_REGION,
        &xfixes_u32s(&[XFIXES_SOURCE, XFIXES_SOURCE2, XFIXES_REGION]),
        &[(10, 20, 30, 40)],
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; IntersectRegion's result must survive that writer's disconnect.
#[test]
fn xfixes_intersect_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    assert_xfixes_single_write_preserves_owner(
        x11xfixes::INTERSECT_REGION,
        &xfixes_u32s(&[XFIXES_SOURCE, XFIXES_SOURCE2, XFIXES_REGION]),
        &[(12, 22, 6, 8)],
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; SubtractRegion's result must survive that writer's disconnect.
#[test]
fn xfixes_subtract_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    assert_xfixes_single_write_preserves_owner(
        x11xfixes::SUBTRACT_REGION,
        &xfixes_u32s(&[XFIXES_SOURCE, XFIXES_SOURCE2, XFIXES_REGION]),
        &[
            (10, 20, 30, 2),
            (10, 22, 2, 8),
            (18, 22, 22, 8),
            (10, 30, 30, 30),
        ],
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; InvertRegion's four output rectangles must survive disconnect.
#[test]
fn xfixes_invert_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    let mut body = XFIXES_SOURCE.to_le_bytes().to_vec();
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&0_i16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    body.extend_from_slice(&100_u16.to_le_bytes());
    body.extend_from_slice(&XFIXES_REGION.to_le_bytes());
    assert_xfixes_single_write_preserves_owner(
        x11xfixes::INVERT_REGION,
        &body,
        &[
            (0, 0, 100, 20),
            (0, 20, 10, 40),
            (40, 20, 60, 40),
            (0, 60, 100, 40),
        ],
    );
}

/// Mutation: make `store_xfixes_region` assign an existing destination to the
/// writer; RegionExtents' rectangle must survive that writer's disconnect.
#[test]
fn xfixes_region_extents_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    assert_xfixes_single_write_preserves_owner(
        x11xfixes::REGION_EXTENTS,
        &xfixes_u32s(&[XFIXES_SOURCE, XFIXES_REGION]),
        &[(10, 20, 30, 40)],
    );
}

/// Mutation: assign the destination's owner to the writer in TranslateRegion;
/// the translated rectangle must remain fetchable after disconnect.
#[test]
fn xfixes_translate_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    let mut body = XFIXES_REGION.to_le_bytes().to_vec();
    body.extend_from_slice(&1_i16.to_le_bytes());
    body.extend_from_slice(&2_i16.to_le_bytes());
    assert_xfixes_single_write_preserves_owner(x11xfixes::TRANSLATE_REGION, &body, &[(2, 4, 3, 4)]);
}

/// Mutation: assign the destination's owner to the writer in ExpandRegion;
/// the expanded rectangle must remain fetchable after disconnect.
#[test]
fn xfixes_expand_region_into_other_clients_region_keeps_owner() {
    use yserver_protocol::x11::xfixes as x11xfixes;

    let mut body = xfixes_u32s(&[XFIXES_SOURCE, XFIXES_REGION]);
    for margin in [1_u16, 2, 3, 4] {
        body.extend_from_slice(&margin.to_le_bytes());
    }
    assert_xfixes_single_write_preserves_owner(x11xfixes::EXPAND_REGION, &body, &[(9, 17, 33, 47)]);
}

/// QueryVersion follows Xorg's rule and gates requests on the
/// negotiated major. Ground truth: Xorg 21.1.24 Xvfb, raw xcb probe —
/// HideCursor before QueryVersion is BadRequest (major 138 there,
/// minor 29, value 0); 4.0 → 4.0; then 2.0 → 2.0 while HideCursor
/// still succeeds and DeletePointerBarrier (a 5.0 request) is
/// BadRequest; 7.0 is capped at the server version.
#[test]
fn xfixes_query_version_negotiates_and_gates_requests() {
    use yserver_protocol::x11::xfixes as x11xfixes;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let root = ROOT_WINDOW.0;

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::HIDE_CURSOR,
        &xfixes_u32s(&[root]),
    );
    let packets = wire_packets(&mut peer);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        error_fields(&packets[0]),
        (x11::error::BAD_REQUEST, 0, 29, XFIXES_MAJOR_OPCODE)
    );
    assert!(state.xfixes_cursor_hide_counts.is_empty());

    let query = |state: &mut ServerState,
                 backend: &mut RecordingBackend,
                 peer: &mut UnixStream,
                 major: u32,
                 minor: u32| {
        xfixes_req(
            state,
            backend,
            1,
            x11xfixes::QUERY_VERSION,
            &xfixes_u32s(&[major, minor]),
        );
        let packets = wire_packets(peer);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][0], 1, "reply");
        (le_u32(&packets[0], 8), le_u32(&packets[0], 12))
    };
    assert_eq!(query(&mut state, &mut backend, &mut peer, 4, 0), (4, 0));
    assert_eq!(query(&mut state, &mut backend, &mut peer, 2, 0), (2, 0));

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::HIDE_CURSOR,
        &xfixes_u32s(&[root]),
    );
    assert!(wire_packets(&mut peer).is_empty(), "4.0 request set kept");
    assert_eq!(state.xfixes_cursor_hide_counts.get(&1), Some(&1));

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::DELETE_POINTER_BARRIER,
        &xfixes_u32s(&[0x1234]),
    );
    let packets = wire_packets(&mut peer);
    assert_eq!(
        error_fields(&packets[0]),
        (x11::error::BAD_REQUEST, 0, 32, XFIXES_MAJOR_OPCODE)
    );

    assert_eq!(query(&mut state, &mut backend, &mut peer, 7, 0), (5, 0));
    assert_eq!(query(&mut state, &mut backend, &mut peer, 3, 7), (3, 7));
}

/// Hide/Show counting, captured on Xvfb: bad window → BadWindow (value
/// = window) for both; ShowCursor without a hide → BadMatch (value =
/// window); two hides need two shows and a third show is BadMatch.
/// Across clients the sprite hides on the first hide anywhere and
/// comes back when the last hider shows or disconnects.
#[test]
fn xfixes_hide_show_cursor_counts_per_client() {
    use yserver_protocol::x11::xfixes as x11xfixes;
    let mut state = ServerState::new();
    let mut peer1 = install_client(&mut state, 1);
    let _peer2 = install_client(&mut state, 2);
    state.xfixes_client_major.insert(1, 5);
    state.xfixes_client_major.insert(2, 5);
    let mut backend = RecordingBackend::new();
    let root = ROOT_WINDOW.0;
    let hidden_calls = |backend: &RecordingBackend| -> Vec<bool> {
        backend
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                RecordedCall::SetCursorHidden(h) => Some(*h),
                _ => None,
            })
            .collect()
    };

    for minor in [x11xfixes::HIDE_CURSOR, x11xfixes::SHOW_CURSOR] {
        xfixes_req(
            &mut state,
            &mut backend,
            1,
            minor,
            &xfixes_u32s(&[0x0bad_bad0]),
        );
        let packets = wire_packets(&mut peer1);
        assert_eq!(
            error_fields(&packets[0]),
            (
                x11::error::BAD_WINDOW,
                0x0bad_bad0,
                u16::from(minor),
                XFIXES_MAJOR_OPCODE
            )
        );
    }
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SHOW_CURSOR,
        &xfixes_u32s(&[root]),
    );
    let packets = wire_packets(&mut peer1);
    assert_eq!(
        error_fields(&packets[0]),
        (x11::error::BAD_MATCH, root, 30, XFIXES_MAJOR_OPCODE)
    );
    assert!(hidden_calls(&backend).is_empty());

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::HIDE_CURSOR,
        &xfixes_u32s(&[root]),
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::HIDE_CURSOR,
        &xfixes_u32s(&[root]),
    );
    xfixes_req(
        &mut state,
        &mut backend,
        2,
        x11xfixes::HIDE_CURSOR,
        &xfixes_u32s(&[root]),
    );
    assert_eq!(
        hidden_calls(&backend),
        vec![true],
        "one edge for three hides"
    );

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SHOW_CURSOR,
        &xfixes_u32s(&[root]),
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SHOW_CURSOR,
        &xfixes_u32s(&[root]),
    );
    assert!(wire_packets(&mut peer1).is_empty());
    assert_eq!(hidden_calls(&backend), vec![true], "client 2 still hides");
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SHOW_CURSOR,
        &xfixes_u32s(&[root]),
    );
    let packets = wire_packets(&mut peer1);
    assert_eq!(
        error_fields(&packets[0]),
        (x11::error::BAD_MATCH, root, 30, XFIXES_MAJOR_OPCODE)
    );

    crate::core_loop::process_disconnect::process_disconnect(&mut state, &mut backend, ClientId(2));
    assert_eq!(
        hidden_calls(&backend),
        vec![true, false],
        "disconnect shows"
    );
    assert!(state.xfixes_cursor_hide_counts.is_empty());
    assert!(!state.xfixes_client_major.contains_key(&2));
}

/// CursorNotify fan-out, per the Xvfb capture: one event per
/// (client, window) selection carrying that window, the cursor serial
/// and its name atom; a destroyed window's selection is gone; bad mask
/// → BadValue, bad window → BadWindow.
#[test]
fn xfixes_cursor_notify_goes_to_each_selection() {
    use yserver_protocol::x11::xfixes as x11xfixes;
    const CHILD: u32 = 0x0040_0001;
    const CURSOR: u32 = 0x0040_0010;
    const HOST: u32 = 0x0001_0077;
    let mut state = ServerState::new();
    let mut peer1 = install_client(&mut state, 1);
    let mut peer2 = install_client(&mut state, 2);
    state.xfixes_client_major.insert(1, 5);
    state.xfixes_client_major.insert(2, 5);
    let mut backend = RecordingBackend::new();
    let root = ROOT_WINDOW.0;
    create_present_test_window(&mut state, CHILD, 0, 0, 10, 10);
    state
        .resources
        .create_cursor(ClientId(1), ResourceId(CURSOR));
    state.resources.set_cursor_host_xid(
        ResourceId(CURSOR),
        crate::backend::CursorHandle::from_raw(HOST).unwrap(),
    );
    let name = state.atoms.intern("bname", false);
    state
        .resources
        .set_cursor_name_atom(ResourceId(CURSOR), name);

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SELECT_CURSOR_INPUT,
        &xfixes_u32s(&[root, 2]),
    );
    assert_eq!(
        error_fields(&wire_packets(&mut peer1)[0]),
        (x11::error::BAD_VALUE, 2, 3, XFIXES_MAJOR_OPCODE)
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SELECT_CURSOR_INPUT,
        &xfixes_u32s(&[0x0bad_bad0, 1]),
    );
    assert_eq!(
        error_fields(&wire_packets(&mut peer1)[0]),
        (x11::error::BAD_WINDOW, 0x0bad_bad0, 3, XFIXES_MAJOR_OPCODE)
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SELECT_CURSOR_INPUT,
        &xfixes_u32s(&[root, 1]),
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::SELECT_CURSOR_INPUT,
        &xfixes_u32s(&[CHILD, 1]),
    );
    assert!(wire_packets(&mut peer1).is_empty());

    backend.displayed_cursor_change = Some(crate::backend::DisplayedCursor {
        host_xid: HOST,
        serial: 3,
    });
    emit_xfixes_cursor_notify(&mut state, &mut backend);
    let events = wire_packets(&mut peer1);
    assert_eq!(events.len(), 2, "one per selection");
    let mut windows = Vec::new();
    for e in &events {
        assert_eq!(e[0], crate::nested::XFIXES_FIRST_EVENT + 1);
        assert_eq!(e[1], x11xfixes::DISPLAY_CURSOR_NOTIFY);
        windows.push(le_u32(e, 4));
        assert_eq!(le_u32(e, 8), 3, "serial");
        assert_eq!(le_u32(e, 16), name.0, "name atom");
    }
    windows.sort_unstable();
    assert_eq!(windows, vec![root, CHILD]);
    assert!(
        wire_packets(&mut peer2).is_empty(),
        "client 2 never selected"
    );
    emit_xfixes_cursor_notify(&mut state, &mut backend);
    assert!(
        wire_packets(&mut peer1).is_empty(),
        "the report is consumed"
    );

    // Destroying the child drops its selection (Xorg CursorFreeWindow).
    destroy_window_subtree(&mut state, &mut backend, None, ResourceId(CHILD)).expect("destroy");
    backend.displayed_cursor_change = Some(crate::backend::DisplayedCursor {
        host_xid: 0x0001_0099,
        serial: 1,
    });
    emit_xfixes_cursor_notify(&mut state, &mut backend);
    let events = wire_packets(&mut peer1);
    assert_eq!(events.len(), 1);
    assert_eq!(le_u32(&events[0], 4), root);
    assert_eq!(le_u32(&events[0], 16), 0, "unnamed cursor");
}

/// ChangeCursor: every XID of the destination cursor now names the
/// source cursor, and the backend replaces its displayed uses. Xvfb:
/// bad source / bad destination → BadCursor naming it.
#[test]
fn xfixes_change_cursor_retargets_cursor_and_backend() {
    use yserver_protocol::x11::xfixes as x11xfixes;
    const A: u32 = 0x0040_0020;
    const D: u32 = 0x0040_0021;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.xfixes_client_major.insert(1, 5);
    let mut backend = RecordingBackend::new();
    for (xid, host) in [(A, 0x0001_0010), (D, 0x0001_0020)] {
        state.resources.create_cursor(ClientId(1), ResourceId(xid));
        state.resources.set_cursor_host_xid(
            ResourceId(xid),
            crate::backend::CursorHandle::from_raw(host).unwrap(),
        );
    }

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR,
        &xfixes_u32s(&[0x0bad_bad0, A]),
    );
    assert_eq!(
        error_fields(&wire_packets(&mut peer)[0]),
        (x11::error::BAD_CURSOR, 0x0bad_bad0, 26, XFIXES_MAJOR_OPCODE)
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR,
        &xfixes_u32s(&[D, 0x0bad_bad0]),
    );
    assert_eq!(
        error_fields(&wire_packets(&mut peer)[0]),
        (x11::error::BAD_CURSOR, 0x0bad_bad0, 26, XFIXES_MAJOR_OPCODE)
    );

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR,
        &xfixes_u32s(&[D, A]),
    );
    assert!(wire_packets(&mut peer).is_empty());
    assert_eq!(
        state.resources.cursor_host_xid(ResourceId(A)),
        Some(0x0001_0020)
    );
    let calls = backend.calls.lock().unwrap().clone();
    assert!(calls.contains(&RecordedCall::ReplaceCursor {
        old_host_xid: 0x0001_0010,
        new_host_xid: 0x0001_0020,
    }));
    // A and D now share one host cursor: freeing one XID must not free it.
    assert_eq!(state.resources.free_cursor(ResourceId(A)), None);
    assert_eq!(
        state.resources.free_cursor(ResourceId(D)),
        Some(0x0001_0020)
    );
}

#[test]
fn xfixes_change_cursor_checks_source_before_resolving_destination() {
    // Mutation: resolve the ChangeCursor destination before checking its source.
    use super::extensions::{ExtensionHook, RecordingExtensionPolicy};
    use crate::access::{AccessMode, DenyError, RequestOpcodes, ResourceKind};
    use yserver_protocol::x11::xfixes as x11xfixes;
    const SOURCE: ResourceId = ResourceId(0x0040_0030);
    const MISSING: u32 = 0x0bad_bad0;
    const ERROR_VALUE: u32 = 0x5a17;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.xfixes_client_major.insert(1, 5);
    state.resources.create_cursor(ClientId(1), SOURCE);
    let mut backend = RecordingBackend::new();
    let source_host_before = state.resources.cursor_host_xid(SOURCE);
    let calls = Arc::new(Mutex::new(Vec::new()));
    state.install_access_policy(Arc::new(RecordingExtensionPolicy::denying_resource(
        Arc::clone(&calls),
        SOURCE,
        Some(AccessMode::READ | AccessMode::GET_ATTR),
        DenyError {
            code: x11::error::BAD_VALUE,
            value: ERROR_VALUE,
        },
    )));

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR,
        &xfixes_u32s(&[SOURCE.0, MISSING]),
    );
    let packets = wire_packets(&mut peer);
    assert_eq!(packets.len(), 1);
    assert_eq!(
        error_fields(&packets[0]),
        (
            x11::error::BAD_VALUE,
            ERROR_VALUE,
            u16::from(x11xfixes::CHANGE_CURSOR),
            XFIXES_MAJOR_OPCODE,
        )
    );
    let recorded = calls.lock().unwrap();
    let checks = recorded
        .iter()
        .filter(|call| call.hook == ExtensionHook::Resource)
        .collect::<Vec<_>>();
    assert_eq!(checks.len(), 1);
    let source = checks[0];
    assert_eq!(source.client, ClientId(1));
    assert_eq!(
        source.target,
        Some(crate::access::ResourceTarget {
            owner: None,
            id: SOURCE,
            kind: ResourceKind::Cursor,
            creation: None,
        })
    );
    assert_eq!(source.mode, Some(AccessMode::READ | AccessMode::GET_ATTR));
    assert_eq!(source.owner, Some(ClientId(1)));
    assert_eq!(source.target_owner, Some(ClientId(1)));
    assert_eq!(
        source.request,
        Some(RequestOpcodes {
            major: XFIXES_MAJOR_OPCODE,
            minor: x11xfixes::CHANGE_CURSOR,
        })
    );
    drop(recorded);
    assert!(backend.calls().is_empty());
    assert!(state.resources.cursor_exists(SOURCE));
    assert!(!state.resources.cursor_exists(ResourceId(MISSING)));
    assert_eq!(state.resources.cursor_host_xid(SOURCE), source_host_before);

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.xfixes_client_major.insert(1, 5);
    state.resources.create_cursor(ClientId(1), SOURCE);
    let mut backend = RecordingBackend::new();
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR,
        &xfixes_u32s(&[SOURCE.0, MISSING]),
    );
    assert_eq!(
        error_fields(&wire_packets(&mut peer)[0]),
        (
            x11::error::BAD_CURSOR,
            MISSING,
            u16::from(x11xfixes::CHANGE_CURSOR),
            XFIXES_MAJOR_OPCODE,
        )
    );
}

/// ChangeCursorByName matches the cursor object's name even after the
/// client freed its XID (the window still shows it); a name nobody
/// interned matches nothing and is not an error (Xvfb: OK).
#[test]
fn xfixes_change_cursor_by_name_replaces_named_cursors() {
    use yserver_protocol::x11::xfixes as x11xfixes;
    const A: u32 = 0x0040_0030;
    const B: u32 = 0x0040_0031;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.xfixes_client_major.insert(1, 5);
    let mut backend = RecordingBackend::new();
    for (xid, host) in [(A, 0x0001_0030), (B, 0x0001_0031)] {
        state.resources.create_cursor(ClientId(1), ResourceId(xid));
        state.resources.set_cursor_host_xid(
            ResourceId(xid),
            crate::backend::CursorHandle::from_raw(host).unwrap(),
        );
    }
    let name = state.atoms.intern("bname", false);
    state.resources.set_cursor_name_atom(ResourceId(B), name);
    let _ = state.resources.free_cursor(ResourceId(B));

    let by_name = |name: &[u8]| {
        let mut body = Vec::new();
        body.extend_from_slice(&A.to_le_bytes());
        body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(name);
        body.resize(body.len().div_ceil(4) * 4, 0);
        body
    };
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR_BY_NAME,
        &by_name(b"zzznoname"),
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::CHANGE_CURSOR_BY_NAME,
        &by_name(b"bname"),
    );
    assert!(wire_packets(&mut peer).is_empty());
    let replaces: Vec<RecordedCall> = backend
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| matches!(c, RecordedCall::ReplaceCursor { .. }))
        .cloned()
        .collect();
    assert_eq!(
        replaces,
        vec![RecordedCall::ReplaceCursor {
            old_host_xid: 0x0001_0031,
            new_host_xid: 0x0001_0030,
        }]
    );
    assert_eq!(state.resources.cursor_name_for_host(0x0001_0031), None);
}

/// ExpandRegion against Xvfb: {(10,10 5x5),(30,30 5x5)} expanded by
/// l1 r2 t3 b4 → {(9,7 8x12),(29,27 8x12)}; by l20 r20 → the
/// overlapping boxes are unioned into {(-10,10 45x5),(10,30 45x5)}; an
/// empty source leaves the destination as it was; bad source or
/// destination → XFixes BadRegion (error base + 0) naming it.
#[test]
fn xfixes_expand_region_matches_xvfb() {
    use yserver_protocol::x11::xfixes::{self as x11xfixes, RegionRect};
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    state.xfixes_client_major.insert(1, 5);
    let mut backend = RecordingBackend::new();
    let rect = |x, y, width, height| RegionRect {
        x,
        y,
        width,
        height,
    };
    let region = |rects: Vec<RegionRect>| crate::server::XFixesRegion {
        owner: ClientId(1),
        region: crate::region::Region::from_rects(&rects),
    };
    state
        .xfixes_regions
        .insert(0x10, region(vec![rect(10, 10, 5, 5), rect(30, 30, 5, 5)]));
    state
        .xfixes_regions
        .insert(0x11, region(vec![rect(100, 100, 7, 7)]));
    state.xfixes_regions.insert(0x12, region(Vec::new()));
    let expand = |src: u32, dst: u32, l: u16, r: u16, t: u16, b: u16| {
        let mut body = xfixes_u32s(&[src, dst]);
        for v in [l, r, t, b] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body
    };

    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::EXPAND_REGION,
        &expand(0x10, 0x11, 1, 2, 3, 4),
    );
    assert_eq!(
        state.xfixes_regions[&0x11].rects(),
        vec![rect(9, 7, 8, 12), rect(29, 27, 8, 12)]
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::EXPAND_REGION,
        &expand(0x10, 0x11, 20, 20, 0, 0),
    );
    assert_eq!(
        state.xfixes_regions[&0x11].rects(),
        vec![rect(-10, 10, 45, 5), rect(10, 30, 45, 5)]
    );
    xfixes_req(
        &mut state,
        &mut backend,
        1,
        x11xfixes::EXPAND_REGION,
        &expand(0x12, 0x11, 1, 1, 1, 1),
    );
    assert_eq!(
        state.xfixes_regions[&0x11].rects(),
        vec![rect(-10, 10, 45, 5), rect(10, 30, 45, 5)]
    );
    assert!(wire_packets(&mut peer).is_empty());

    for (src, dst) in [(0x0bad_bad0, 0x11), (0x10, 0x0bad_bad0)] {
        xfixes_req(
            &mut state,
            &mut backend,
            1,
            x11xfixes::EXPAND_REGION,
            &expand(src, dst, 1, 1, 1, 1),
        );
        assert_eq!(
            error_fields(&wire_packets(&mut peer)[0]),
            (
                crate::nested::XFIXES_FIRST_ERROR,
                0x0bad_bad0,
                28,
                XFIXES_MAJOR_OPCODE
            )
        );
    }
}

fn xfixes_create_barrier(
    state: &mut ServerState,
    client: ClientId,
    barrier: u32,
    window: u32,
    x1: i16,
    y1: i16,
    x2: i16,
    y2: i16,
    directions: u32,
    devices: &[u16],
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::xfixes as x11xfixes;

    let mut body = Vec::new();
    body.extend_from_slice(&barrier.to_le_bytes());
    body.extend_from_slice(&window.to_le_bytes());
    body.extend_from_slice(&x1.to_le_bytes());
    body.extend_from_slice(&y1.to_le_bytes());
    body.extend_from_slice(&x2.to_le_bytes());
    body.extend_from_slice(&y2.to_le_bytes());
    body.extend_from_slice(&directions.to_le_bytes());
    body.extend_from_slice(&0u16.to_le_bytes());
    body.extend_from_slice(&(devices.len() as u16).to_le_bytes());
    for &device in devices {
        body.extend_from_slice(&device.to_le_bytes());
    }

    let header = RequestHeader {
        opcode: 140,
        data: x11xfixes::CREATE_POINTER_BARRIER,
        length_units: u32::try_from(1 + body.len() / 4).unwrap(),
    };
    handle_xfixes_request(
        state,
        &mut RecordingBackend::new(),
        None,
        client,
        SequenceNumber(1),
        header,
        &body,
    )
}

fn xfixes_delete_barrier(
    state: &mut ServerState,
    client: ClientId,
    barrier: u32,
) -> io::Result<RequestOutcome> {
    use yserver_protocol::x11::xfixes as x11xfixes;
    let body = barrier.to_le_bytes();
    let header = RequestHeader {
        opcode: 140,
        data: x11xfixes::DELETE_POINTER_BARRIER,
        length_units: 2,
    };
    handle_xfixes_request(
        state,
        &mut RecordingBackend::new(),
        None,
        client,
        SequenceNumber(1),
        header,
        &body,
    )
}

fn xi2_barrier_release(
    state: &mut ServerState,
    client: ClientId,
    entries: &[(u16, u32, u32)],
) -> io::Result<RequestOutcome> {
    let mut body = Vec::new();
    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for &(deviceid, barrier, eventid) in entries {
        body.extend_from_slice(&deviceid.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&barrier.to_le_bytes());
        body.extend_from_slice(&eventid.to_le_bytes());
    }
    handle_xi2_request(
        state,
        &mut RecordingBackend::new(),
        None,
        client,
        SequenceNumber(1),
        xi2_header_for_body(61, &body),
        &body,
    )
}

#[test]
fn create_pointer_barrier_stores_resource() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let bid = 0x0040_0001u32;

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        bid,
        ROOT_WINDOW.0,
        100,
        0,
        100,
        200,
        0,
        &[],
    )
    .expect("create barrier");

    let b = state.pointer_barriers.get(&bid).expect("stored");
    assert_eq!((b.x1, b.y1, b.x2, b.y2), (100, 0, 100, 200));
    assert_eq!(b.event_id, 1);
    assert_eq!(b.release_event_id, 0);
    assert!(read_all_available(&mut peer).is_empty());
}

#[test]
fn create_pointer_barrier_diagonal_is_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let bid = 0x0040_0001u32;

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        bid,
        ROOT_WINDOW.0,
        0,
        0,
        50,
        80,
        0,
        &[],
    )
    .expect("request handled");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
    assert!(!state.pointer_barriers.contains_key(&bid));
}

#[test]
fn create_pointer_barrier_bad_window() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        0x0040_0001,
        0x9999,
        100,
        0,
        100,
        200,
        0,
        &[],
    )
    .expect("request handled");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_WINDOW);
}

#[test]
fn create_pointer_barrier_negative_on_fixed_axis_is_bad_value() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        0x0040_0001,
        ROOT_WINDOW.0,
        -1,
        0,
        -1,
        200,
        0,
        &[],
    )
    .expect("request handled");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], x11::error::BAD_VALUE);
}

#[test]
fn create_pointer_barrier_bad_device() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        0x0040_0001,
        ROOT_WINDOW.0,
        100,
        0,
        100,
        200,
        0,
        &[3],
    )
    .expect("request handled");

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes[1], XI2_FIRST_ERROR);
}

#[test]
fn delete_pointer_barrier_frees_and_owner_checks() {
    let mut state = ServerState::new();
    let mut peer1 = install_client(&mut state, 1);
    let mut peer2 = install_client(&mut state, 2);
    let bid = 0x0040_0001u32;

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        bid,
        ROOT_WINDOW.0,
        100,
        0,
        100,
        200,
        0,
        &[],
    )
    .expect("create barrier");
    assert!(read_all_available(&mut peer1).is_empty());

    xfixes_delete_barrier(&mut state, ClientId(2), bid).expect("wrong-client delete handled");
    let bytes = read_all_available(&mut peer2);
    assert_eq!(bytes[1], x11::error::BAD_ACCESS);
    assert!(state.pointer_barriers.contains_key(&bid));

    xfixes_delete_barrier(&mut state, ClientId(1), bid).expect("owner delete handled");
    assert!(!state.pointer_barriers.contains_key(&bid));
}

#[test]
fn xi2_barrier_release_arms_matching_barrier() {
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let bid = 0x0040_0002u32;

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        bid,
        ROOT_WINDOW.0,
        100,
        0,
        100,
        200,
        0,
        &[],
    )
    .expect("create barrier");

    xi2_barrier_release(&mut state, ClientId(1), &[(2, bid, 1)]).expect("release handled");

    let barrier = state.pointer_barriers.get(&bid).expect("barrier");
    assert_eq!(barrier.release_event_id, 1);
}

#[test]
fn delete_while_hit_emits_released_leave() {
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    if let Some(client) = state.clients.get_mut(&1) {
        client.xi2_masks.insert((ROOT_WINDOW, 2), 1 << 26);
    }
    let bid = 0x0040_0003u32;

    xfixes_create_barrier(
        &mut state,
        ClientId(1),
        bid,
        ROOT_WINDOW.0,
        100,
        0,
        100,
        200,
        0,
        &[],
    )
    .expect("create barrier");
    {
        let barrier = state.pointer_barriers.get_mut(&bid).expect("barrier");
        barrier.hit = true;
        barrier.event_id = 1;
        barrier.last_timestamp = 10;
    }
    // Sprite resting on the barrier. The released leave must carry the
    // CURRENT position (not 0,0 — the pre-fix bug).
    state.pointer_root = (100, 50);

    xfixes_delete_barrier(&mut state, ClientId(1), bid).expect("owner delete handled");
    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 68);
    assert_eq!(bytes[0], 35, "GenericEvent");
    assert_eq!(bytes[1], XI2_MAJOR_OPCODE, "XI2 major opcode");
    assert_eq!(&bytes[8..10], &26u16.to_le_bytes(), "BarrierLeave");
    assert_eq!(
        &bytes[36..40],
        &1u32.to_le_bytes(),
        "XIBarrierPointerReleased"
    );
    assert_eq!(&bytes[40..42], &0u16.to_le_bytes(), "sourceid = 0");
    // root_x/root_y carry the current sprite position as FP1616 (v<<16),
    // not the pre-fix 0,0.
    assert_eq!(
        &bytes[44..48],
        &(100i32 << 16).to_le_bytes(),
        "root_x = 100"
    );
    assert_eq!(&bytes[48..52], &(50i32 << 16).to_le_bytes(), "root_y = 50");
    assert!(!state.pointer_barriers.contains_key(&bid));
}

#[test]
fn shape_input_change_emits_shape_notify_to_selectors() {
    // A client that ShapeSelectInput'd a window must receive a
    // ShapeNotify when that window's input region changes. Regression
    // for the cinnamon "nemo rises" bug: nautilus grows its input shape
    // from a tiny startup rect to full size; without ShapeNotify the
    // compositing WM keeps the stale tiny region, treats the window as
    // click-through over most of its area, and focuses/raises the
    // window below on a click.
    use yserver_protocol::x11::{shape as x11shape, xfixes::RegionRect};
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let win = ResourceId(0x1b00004);
    seed_window(&mut state, win, ROOT_WINDOW, 800, 600);

    // WM selected ShapeNotify on the window.
    state.shape_select_masks.insert((1, win), true);

    // Window sets its (grown) input shape, then we notify.
    crate::nested::set_shape_rects(
        &mut state,
        win,
        x11shape::KIND_INPUT,
        vec![RegionRect {
            x: 13,
            y: 13,
            width: 1482,
            height: 1024,
        }],
    );
    emit_shape_notify(&mut state, win, x11shape::KIND_INPUT);

    let bytes = read_all_available(&mut peer);
    assert_eq!(bytes.len(), 32, "one 32-byte ShapeNotify");
    assert_eq!(
        bytes[0],
        crate::nested::SHAPE_FIRST_EVENT,
        "ShapeNotify type"
    );
    assert_eq!(bytes[1], x11shape::KIND_INPUT, "kind = Input");
    assert_eq!(&bytes[4..8], &win.0.to_le_bytes(), "affected window");
    assert_eq!(bytes[20], 1, "shaped = true");

    // A client that did NOT select gets nothing.
    let mut peer2 = install_client(&mut state, 2);
    emit_shape_notify(&mut state, win, x11shape::KIND_INPUT);
    assert!(
        read_all_available(&mut peer2).is_empty(),
        "non-selecting client must not receive ShapeNotify"
    );
}

/// A `ShapeMask(kind=Input, src=None)` that clears an already-unset
/// input shape is a no-op and must NOT generate a ShapeNotify —
/// matching Xorg. xfwm4 re-asserts exactly this on its panel frames
/// repeatedly; a spurious notify each time made it re-derive the
/// frame's input shape from a stale geometry-default rect, so XFCE
/// bottom-panel buttons fell through to xfdesktop (HW xfce
/// 2026-06-22). Drives the real `handle_shape_request` MASK path.
#[test]
fn shape_mask_none_clearing_unset_input_emits_no_shape_notify() {
    use yserver_protocol::x11::shape as x11shape;
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let win = ResourceId(0x1b00004);
    seed_window(&mut state, win, ROOT_WINDOW, 25, 49);
    // WM (xfwm4-like) selected ShapeNotify on the window.
    state.shape_select_masks.insert((1, win), true);

    // MASK body: op(1) dest_kind(1) pad(2) dest(4) x_off(2) y_off(2) src(4).
    let mut body = vec![0u8; 16];
    body[0] = x11shape::OP_SET;
    body[1] = x11shape::KIND_INPUT;
    body[4..8].copy_from_slice(&win.0.to_le_bytes());
    // src stays 0 == None → clear the (already-unset) input shape.
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 129, // SHAPE major (unused by handle_shape_request)
        data: 2,     // SHAPE minor: Mask
        length_units: 5,
    };
    handle_shape_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("ShapeMask");

    assert!(
        read_all_available(&mut peer).is_empty(),
        "no-op input clear must emit no ShapeNotify",
    );
}

/// XFIXES SUBTRACT_REGION partial-overlap: subtracting a
/// region that overlaps part of `a` must return the bands of
/// `a` *not* covered by `b`. The pre-fix dispatcher collapsed
/// any overlap to an empty result; compositors that build
/// "screen MINUS windows" wallpaper clips this way would end
/// up with no clip at all.
#[test]
fn xfixes_subtract_region_partial_overlap_returns_remaining_bands() {
    use crate::server::XFixesRegion;
    use yserver_protocol::x11::xfixes::RegionRect;
    let mut state = ServerState::new();
    let mut backend = RecordingBackend::new();
    // Region A: a 100x100 square at the origin.
    state.xfixes_regions.insert(
        0x10,
        XFixesRegion {
            owner: ClientId(1),
            region: crate::region::Region::from_rects(&[RegionRect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }]),
        },
    );
    // Region B: a 50x100 strip overlapping the right half of A.
    state.xfixes_regions.insert(
        0x11,
        XFixesRegion {
            owner: ClientId(1),
            region: crate::region::Region::from_rects(&[RegionRect {
                x: 50,
                y: 0,
                width: 50,
                height: 100,
            }]),
        },
    );
    // SUBTRACT_REGION body: source1(4) + source2(4) + dest(4).
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&0x10u32.to_le_bytes());
    body.extend_from_slice(&0x11u32.to_le_bytes());
    body.extend_from_slice(&0x12u32.to_le_bytes());
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(1, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(1),
        SequenceNumber(1),
        RequestHeader {
            opcode: 140, // XFIXES major
            data: yserver_protocol::x11::xfixes::SUBTRACT_REGION,
            length_units: 4,
        },
        &body,
        None,
    )
    .unwrap();

    let result = state
        .xfixes_regions
        .get(&0x12)
        .expect("dest region recorded");
    assert!(
        !result.rects().is_empty(),
        "partial-overlap subtraction must NOT collapse to empty — \
             pre-fix this returns Vec::new() and breaks compositor \
             wallpaper-clip computation",
    );
    // Geometrically: A (0,0 100x100) minus B (50,0 50x100)
    // leaves the left band (0,0 50x100). Allow any equivalent
    // rect decomposition that covers exactly that area.
    let total_area: u64 = result
        .rects()
        .iter()
        .map(|r| u64::from(r.width) * u64::from(r.height))
        .sum();
    assert_eq!(
        total_area,
        50 * 100,
        "result area must be 5000 (the left half of A); got rects {:?}",
        result.rects(),
    );
    // No result rect may extend into B's x-range [50, 100).
    for r in &result.rects() {
        assert!(
            r.x + i16::try_from(r.width).unwrap_or(i16::MAX) <= 50,
            "result rect {r:?} extends into the subtracted region",
        );
    }
}

/// Audit #7 (code-review follow-up): Xorg's
/// `ProcXFixesCreateRegionFromPicture` (`xfixes/region.c:257-258`)
/// returns `RenderErrBase + BadPicture` when the picture has no
/// underlying drawable. yserver models this case as
/// `PictureKind::Sourceless` (SolidFill / gradient pictures).
/// Pre-fix the handler fell through to `picture_client_clip_rects`
/// and surfaced BadMatch — wrong error class for a sourceless
/// picture.
#[test]
fn create_region_from_picture_with_sourceless_picture_returns_bad_picture() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;
    const APP: u32 = 84;
    const PICTURE_XID: u32 = 0x0084_0001;
    const REGION_XID: u32 = 0x0084_0002;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    state.resources.create_picture(
        ResourceId(PICTURE_XID),
        crate::resources::PictureState {
            client: ClientId(APP),
            host_picture_xid: Some(crate::backend::PictureHandle::from_raw_for_test(0x42)),
            host_owned_pixmap: None,
            kind: crate::resources::PictureKind::Sourceless,
            drawable: None,
            window: None,
        },
    );

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    body.extend_from_slice(&PICTURE_XID.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION_FROM_PICTURE,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(
        buf[1],
        crate::nested::RENDER_FIRST_ERROR + 1,
        "expected BadPicture (Render first_error + 1 = 153) for \
             sourceless picture; got {}",
        buf[1],
    );
    assert!(
        !state.xfixes_regions.contains_key(&REGION_XID),
        "no region must be inserted on BadPicture",
    );
}

/// Audit #7 (code-review follow-up): Xorg's
/// `ProcXFixesCreateRegionFromPicture` uses `VERIFY_PICTURE`
/// (`xfixes/region.c:255`) which returns
/// `RenderErrBase + BadPicture` for an unknown picture xid,
/// not BadDrawable. yserver was emitting BadDrawable.
#[test]
fn create_region_from_picture_with_unknown_picture_returns_bad_picture() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;
    const APP: u32 = 85;
    const UNKNOWN_PIC: u32 = 0x0085_dead;
    const REGION_XID: u32 = 0x0085_0001;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    body.extend_from_slice(&UNKNOWN_PIC.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION_FROM_PICTURE,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(
        buf[1],
        crate::nested::RENDER_FIRST_ERROR + 1,
        "expected BadPicture for unknown picture xid (Xorg VERIFY_PICTURE \
             returns RenderErrBase+BadPicture, NOT BadDrawable); got {}",
        buf[1],
    );
}

/// Audit #7 (code-review follow-up): Xorg's
/// `ProcXFixesCreateRegionFromWindow` (`xfixes/region.c:158-163`)
/// returns BadWindow when `dixLookupResourceByType` on the
/// window xid fails, with `client->error_value = stuff->window`.
/// Pre-fix yserver passed any xid (including 0) through to
/// `shape_rects_for` and silently inserted a default/empty
/// region.
#[test]
fn create_region_from_window_with_unknown_window_returns_bad_window() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;
    const APP: u32 = 82;
    const REGION_XID: u32 = 0x0082_0001;
    const UNKNOWN_WIN: u32 = 0x0082_ffff;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    // body: region(4) + window(4) + kind(1) + pad(3)
    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    body.extend_from_slice(&UNKNOWN_WIN.to_le_bytes());
    body.push(0); // kind = Bounding (valid)
    body.extend_from_slice(&[0u8; 3]);
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION_FROM_WINDOW,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_WINDOW,
        "expected BadWindow (3) for unknown window xid; got {}",
        buf[1],
    );
    assert_eq!(
        u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        UNKNOWN_WIN,
        "error.bad_value must carry the offending window xid",
    );
    assert!(
        !state.xfixes_regions.contains_key(&REGION_XID),
        "no region must be inserted on BadWindow",
    );
}

/// Audit #7 (code-review follow-up): Xorg's
/// `ProcXFixesCreateRegionFromWindow` (`xfixes/region.c:164-181`)
/// accepts only `WindowRegionBounding` (0) and `WindowRegionClip`
/// (1); any other `kind` returns BadValue with
/// `client->error_value = stuff->kind`. The SHAPE-shaped value 2
/// (Input) is rejected per X11 XFIXES spec.
#[test]
fn create_region_from_window_with_invalid_kind_returns_bad_value() {
    use std::io::Read;
    use yserver_protocol::x11::{CreateWindowRequest, xfixes as x11xfixes};
    const APP: u32 = 83;
    const REGION_XID: u32 = 0x0083_0001;
    const WIN_XID: u32 = 0x0083_0002;
    const INVALID_KIND: u8 = 2;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();
    // Window must exist so the BadWindow gate doesn't fire
    // first — we want to isolate the BadValue(kind) path.
    state.resources.create_window(
        ClientId(APP),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );

    let mut body = Vec::with_capacity(12);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    body.extend_from_slice(&WIN_XID.to_le_bytes());
    body.push(INVALID_KIND);
    body.extend_from_slice(&[0u8; 3]);
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION_FROM_WINDOW,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_VALUE,
        "expected BadValue (2) for kind={INVALID_KIND}; got {}",
        buf[1],
    );
    assert_eq!(
        u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
        u32::from(INVALID_KIND),
        "error.bad_value must carry the offending kind",
    );
    assert!(
        !state.xfixes_regions.contains_key(&REGION_XID),
        "no region must be inserted on BadValue",
    );
}

/// Audit #7 (code-review follow-up): every XFIXES
/// `CreateRegion*` opcode begins with
/// `LEGAL_NEW_RESOURCE(stuff->region, client)` in Xorg
/// (`xfixes/region.c:78,114,157,213,…`). Reusing an existing
/// region XID must surface BadIDChoice. Pre-fix yserver silently
/// overwrote `state.xfixes_regions[region]` via `.insert(…)`,
/// matching the colormap/window/etc. bug class that the
/// codebase already gates elsewhere.
#[test]
fn create_region_with_in_use_xid_returns_bad_id_choice() {
    use std::io::Read;
    use yserver_protocol::x11::xfixes as x11xfixes;
    const APP: u32 = 80;
    const REGION_XID: u32 = 0x0080_0001;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    // Pre-populate the region xid so the new request is a
    // duplicate.
    state.xfixes_regions.insert(
        REGION_XID,
        crate::server::XFixesRegion {
            owner: ClientId(APP),
            region: crate::region::Region::default(),
        },
    );

    // CreateRegion body: region(4) + N×8 (rects). Zero rects =
    // empty region; the duplicate-xid check must fire BEFORE
    // any rect parsing.
    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(buf[0], 0, "first byte = error class");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_ID_CHOICE,
        "expected BadIDChoice (14) when reusing an existing region xid; got {}",
        buf[1],
    );
    // The duplicate must not be overwritten — the pre-existing
    // entry's marker (empty rects placeholder) survives intact.
    let r = state
        .xfixes_regions
        .get(&REGION_XID)
        .expect("pre-existing region must remain");
    assert!(
        r.rects().is_empty(),
        "pre-existing region must not be overwritten on BadIDChoice; \
             got rects={:?}",
        r.rects(),
    );
}

/// Audit #7 (code-review follow-up): the duplicate-xid gate
/// also applies to `CREATE_REGION_FROM_GC` (and all other
/// CreateRegion variants). The GC path is the audit-#7 patch
/// site — confirm the new gate is wired in front of the raw
/// clip-copy too.
#[test]
fn create_region_from_gc_with_in_use_xid_returns_bad_id_choice() {
    use std::io::Read;
    use yserver_protocol::x11::{
        ClipRectangles, CreateGcRequest, CreateWindowRequest, SetClipRectanglesRequest,
        xfixes as x11xfixes,
    };
    const APP: u32 = 81;
    const WIN_XID: u32 = 0x0081_0001;
    const GC_XID: u32 = 0x0081_0002;
    const REGION_XID: u32 = 0x0081_0003;

    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();
    state.resources.create_window(
        ClientId(APP),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_gc(
        ClientId(APP),
        CreateGcRequest {
            gc: ResourceId(GC_XID),
            drawable: ResourceId(WIN_XID),
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );
    // Single-rect clip so the GC's `clientClip` exists.
    let mut rect_bytes = Vec::with_capacity(8);
    rect_bytes.extend_from_slice(&0i16.to_le_bytes());
    rect_bytes.extend_from_slice(&0i16.to_le_bytes());
    rect_bytes.extend_from_slice(&5u16.to_le_bytes());
    rect_bytes.extend_from_slice(&5u16.to_le_bytes());
    state.resources.set_clip_rectangles(
        yserver_protocol::x11::ClientId(1),
        SetClipRectanglesRequest {
            gc: ResourceId(GC_XID),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: rect_bytes,
            },
        },
    );

    // Pre-occupy REGION_XID.
    state.xfixes_regions.insert(
        REGION_XID,
        crate::server::XFixesRegion {
            owner: ClientId(APP),
            region: crate::region::Region::default(),
        },
    );

    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    body.extend_from_slice(&GC_XID.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION_FROM_GC,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    peer.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 32];
    peer.read_exact(&mut buf).expect("error reply delivered");
    assert_eq!(
        buf[1],
        yserver_protocol::x11::error::BAD_ID_CHOICE,
        "CreateRegionFromGC must gate on LEGAL_NEW_RESOURCE before \
             copying the GC's clip; got error code {}",
        buf[1],
    );
}

/// Audit #7 (`docs/protocol-audit-2026-05-19.md`):
/// `CreateRegionFromGC` must return the GC's clip rectangles in
/// their RAW clip-coordinate space — Xorg's
/// `ProcXFixesCreateRegionFromGC` (`xfixes/region.c:219-226`)
/// just `XFixesRegionCopy(pGC->clientClip)` with NO origin
/// translation applied. yserver was offsetting by
/// `(clip.x_origin, clip.y_origin)` at insert time. When a WM
/// later re-installs the resulting region as a GC's clip via
/// `SetGcClipRegion(gc, region, x_origin, y_origin)`, the
/// GC-side origin is applied a SECOND time → double-translation
/// → wrong area shadowed/repainted.
#[test]
fn create_region_from_gc_copies_clip_rects_without_origin_translation() {
    use yserver_protocol::x11::{
        ClipRectangles, CreateGcRequest, CreateWindowRequest, SetClipRectanglesRequest,
        xfixes as x11xfixes,
    };
    const APP: u32 = 70;
    const WIN_XID: u32 = 0x0070_0001;
    const GC_XID: u32 = 0x0070_0002;
    const REGION_XID: u32 = 0x0070_0003;
    const X_ORIGIN: i16 = 100;
    const Y_ORIGIN: i16 = 200;
    const RECT_X: i16 = 10;
    const RECT_Y: i16 = 20;
    const RECT_W: u16 = 5;
    const RECT_H: u16 = 5;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, APP);
    let mut backend = RecordingBackend::new();

    // Drawable to attach the GC to.
    state.resources.create_window(
        ClientId(APP),
        CreateWindowRequest {
            depth: 24,
            window: ResourceId(WIN_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state.resources.create_gc(
        ClientId(APP),
        CreateGcRequest {
            gc: ResourceId(GC_XID),
            drawable: ResourceId(WIN_XID),
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        },
    );

    // Set the GC's clip: single rect at (10, 20, 5×5) in
    // clip-coordinate space, with origin (100, 200). The wire
    // format is (x i16, y i16, w u16, h u16) per rect.
    let mut rect_bytes = Vec::with_capacity(8);
    rect_bytes.extend_from_slice(&RECT_X.to_le_bytes());
    rect_bytes.extend_from_slice(&RECT_Y.to_le_bytes());
    rect_bytes.extend_from_slice(&RECT_W.to_le_bytes());
    rect_bytes.extend_from_slice(&RECT_H.to_le_bytes());
    state.resources.set_clip_rectangles(
        yserver_protocol::x11::ClientId(1),
        SetClipRectanglesRequest {
            gc: ResourceId(GC_XID),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: X_ORIGIN,
                y_origin: Y_ORIGIN,
                rectangles: rect_bytes,
            },
        },
    );

    // CREATE_REGION_FROM_GC body: region(4) + gc(4).
    let mut body = Vec::with_capacity(8);
    body.extend_from_slice(&REGION_XID.to_le_bytes());
    body.extend_from_slice(&GC_XID.to_le_bytes());
    let length_units = u32::try_from(1 + body.len().div_ceil(4)).expect("body fits");
    // The client negotiated XFIXES (Xorg gates requests on QueryVersion).
    state.xfixes_client_major.insert(APP, 5);
    process_request(
        &mut state,
        &mut backend,
        ClientId(APP),
        SequenceNumber(1),
        RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: x11xfixes::CREATE_REGION_FROM_GC,
            length_units,
        },
        &body,
        None,
    )
    .expect("process_request");

    let region = state
        .xfixes_regions
        .get(&REGION_XID)
        .expect("CreateRegionFromGC inserted region");
    assert_eq!(
        region.rects().len(),
        1,
        "expected exactly one rect from the GC's single-rect clip; got {:?}",
        region.rects(),
    );
    let r = &region.rects()[0];
    assert_eq!(
        (r.x, r.y, r.width, r.height),
        (RECT_X, RECT_Y, RECT_W, RECT_H),
        "rect must be in raw clip-coord space (NOT translated by \
             clip_origin=({X_ORIGIN},{Y_ORIGIN})); the symptom of \
             over-translation is x={}, y={} (= raw + clip_origin).",
        r.x,
        r.y,
    );
}

// DRIFT 1 + Multi-monitor Bug A regression: the Bounding-shape mirror
// must distinguish three states via the Option API —
//   unset    → None    (drop the entry; scene tracks live geometry —
//                        the multi-monitor Bug A fix), NOT the
//                        materialized default geometry rect;
//   empty    → Some(0)  (explicit empty region — distinct from unset);
//   concrete → Some(n)  (the real rects).
#[test]
fn bounding_shape_mirror_distinguishes_unset_empty_and_concrete() {
    use yserver_protocol::x11::shape as x11shape;

    const WINDOW_XID: u32 = 0x0010_0001;
    const HOST_XID: u32 = 0x0040_0001;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(WINDOW_XID))
        .expect("window installed")
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));

    // No explicit Bounding shape set → mirror must push empty rects.
    mirror_shape_to_host_state(
        &state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        x11shape::KIND_BOUNDING,
    );
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_BOUNDING,
                rects: None,
            }
        ),
        "unset Bounding shape must mirror None (drop backend entry), \
             not the materialized 2560x1440 default geometry rect, and NOT \
             an explicit empty region (Some(0)) — DRIFT 1 distinction",
    );

    // An explicitly-set EMPTY Bounding shape is a DISTINCT state from
    // unset: it mirrors Some(0), not None. This is the DRIFT 1 fix —
    // the old &[]-based API collapsed both to a zero-length call.
    crate::nested::set_shape_rects(
        &mut state,
        ResourceId(WINDOW_XID),
        x11shape::KIND_BOUNDING,
        vec![],
    );
    mirror_shape_to_host_state(
        &state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        x11shape::KIND_BOUNDING,
    );
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_BOUNDING,
                rects: Some(0),
            }
        ),
        "explicit EMPTY Bounding shape must mirror Some(0) (an empty \
             region), distinct from unset's None",
    );

    // After an explicit non-empty Bounding shape, mirror the concrete rect(s).
    crate::nested::set_shape_rects(
        &mut state,
        ResourceId(WINDOW_XID),
        x11shape::KIND_BOUNDING,
        vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }],
    );
    mirror_shape_to_host_state(
        &state,
        &mut backend,
        None,
        ResourceId(WINDOW_XID),
        x11shape::KIND_BOUNDING,
    );
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_BOUNDING,
                rects: Some(1),
            }
        ),
        "explicitly-set Bounding shape must mirror the concrete rect",
    );
}

/// Cinnamon's lock screen, as muffin sends it: an XFIXES region that
/// `InvertRegion` empties, set as the COW's Bounding shape, must reach the
/// backend as an explicit EMPTY region, and region None as unset — the
/// two stay distinct (`ProcXFixesSetWindowShapeRegion`, `xfixes/region.c`:
/// a NULL region pointer unshapes, a copied empty region shapes to
/// nothing).
#[test]
fn xfixes_set_window_shape_region_keeps_empty_distinct_from_none() {
    use yserver_protocol::x11::{shape as x11shape, xfixes as x11xfixes};

    const REGION: u32 = 0x0010_0042;
    let cow = crate::resources::COMPOSITE_OVERLAY_WINDOW;
    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    state
        .resources
        .materialize_cow_resource(crate::backend::WindowHandle::from_raw_for_test(cow.0));

    let mut send = |state: &mut ServerState, minor: u8, body: Vec<u8>| {
        let header = yserver_protocol::x11::RequestHeader {
            opcode: XFIXES_MAJOR_OPCODE,
            data: minor,
            length_units: u32::try_from(1 + body.len() / 4).unwrap(),
        };
        handle_xfixes_request(
            state,
            &mut backend,
            None,
            ClientId(1),
            SequenceNumber(1),
            header,
            &body,
        )
        .expect("XFIXES request");
    };
    let full = [0_i16, 0, 5120, 1440]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect::<Vec<u8>>();
    let shape_body = |region: u32| {
        let mut body = cow.0.to_le_bytes().to_vec();
        body.extend_from_slice(&[x11shape::KIND_BOUNDING, 0, 0, 0, 0, 0, 0, 0]);
        body.extend_from_slice(&region.to_le_bytes());
        body
    };

    let mut create = REGION.to_le_bytes().to_vec();
    create.extend_from_slice(&full);
    send(&mut state, x11xfixes::CREATE_REGION, create);
    let mut invert = REGION.to_le_bytes().to_vec();
    invert.extend_from_slice(&full);
    invert.extend_from_slice(&REGION.to_le_bytes());
    send(&mut state, x11xfixes::INVERT_REGION, invert);
    send(
        &mut state,
        x11xfixes::SET_WINDOW_SHAPE_REGION,
        shape_body(REGION),
    );
    assert!(crate::nested::shape_kind_is_set(
        &state,
        cow,
        x11shape::KIND_BOUNDING
    ));
    assert!(crate::nested::shape_rects_for(&state, cow, x11shape::KIND_BOUNDING).is_empty());

    send(
        &mut state,
        x11xfixes::SET_WINDOW_SHAPE_REGION,
        shape_body(0),
    );
    assert!(!crate::nested::shape_kind_is_set(
        &state,
        cow,
        x11shape::KIND_BOUNDING
    ));

    let mirrored: Vec<_> = backend
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid,
                kind: x11shape::KIND_BOUNDING,
                rects,
            } if host_xid == cow.0 => Some(rects),
            _ => None,
        })
        .collect();
    assert_eq!(mirrored, vec![Some(0), None]);
}

// #133: the CLIP mirror must make the same unset/empty/concrete
// distinction as Bounding. Before step 5 the scene only consulted the
// bounding shape, so freezing the geometry rect for an unset clip shape
// was inert; step 5 clips descendants to the parent's clip shape, and a
// frozen rect then truncates the children after the parent resizes. That
// is the wezterm white block: awesome resets its frame's clip shape at
// 820x583, the frame later becomes 608x734, and 168 rows of the client
// vanished. Input is deliberately NOT in the guard — it feeds the cursor
// hit-test, which wants a concrete region.
#[test]
fn clip_shape_mirror_leaves_an_unset_region_unmaterialized() {
    use yserver_protocol::x11::shape as x11shape;

    const WINDOW_XID: u32 = 0x0010_0002;
    const HOST_XID: u32 = 0x0040_0002;

    let mut state = ServerState::new();
    let _peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();

    state.resources.create_window(
        yserver_protocol::x11::ClientId(1),
        yserver_protocol::x11::CreateWindowRequest {
            depth: 24,
            window: ResourceId(WINDOW_XID),
            parent: crate::resources::ROOT_WINDOW,
            x: 0,
            y: 0,
            width: 820,
            height: 583,
            border_width: 0,
            class: 1,
            visual: crate::resources::ROOT_VISUAL,
            ..Default::default()
        },
    );
    state
        .resources
        .window_mut(ResourceId(WINDOW_XID))
        .expect("window installed")
        .host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(HOST_XID));

    let mirror = |state: &ServerState, backend: &mut RecordingBackend, kind: u8| {
        mirror_shape_to_host_state(state, backend, None, ResourceId(WINDOW_XID), kind);
    };

    mirror(&state, &mut backend, x11shape::KIND_CLIP);
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_CLIP,
                rects: None,
            }
        ),
        "an unset Clip shape must mirror None, not the 820x583 geometry \
             rect that goes stale on the next resize",
    );

    // Input keeps materializing the default rect — the cursor hit-test
    // consumes a concrete region and does not clip descendants.
    mirror(&state, &mut backend, x11shape::KIND_INPUT);
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_INPUT,
                rects: Some(1),
            }
        ),
        "an unset Input shape still mirrors the default geometry rect",
    );

    // An explicit EMPTY clip region is a distinct state from unset.
    crate::nested::set_shape_rects(
        &mut state,
        ResourceId(WINDOW_XID),
        x11shape::KIND_CLIP,
        vec![],
    );
    mirror(&state, &mut backend, x11shape::KIND_CLIP);
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_CLIP,
                rects: Some(0),
            }
        ),
        "explicit EMPTY Clip shape mirrors Some(0), distinct from unset",
    );

    // And a real clip shape is still mirrored through.
    crate::nested::set_shape_rects(
        &mut state,
        ResourceId(WINDOW_XID),
        x11shape::KIND_CLIP,
        vec![yserver_protocol::x11::xfixes::RegionRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }],
    );
    mirror(&state, &mut backend, x11shape::KIND_CLIP);
    assert_eq!(
        backend.calls().last(),
        Some(
            &crate::backend::recording::RecordedCall::SetShapeRectangles {
                host_xid: HOST_XID,
                kind: x11shape::KIND_CLIP,
                rects: Some(1),
            }
        ),
        "explicitly-set Clip shape must mirror the concrete rect",
    );
}

/// GDK clips a native child of a client-side window with its bounding
/// shape and shifts the shape when it scrolls. Xorg's `miSetShape`
/// (`mi/miwindow.c:637-677`) then exposes what the window no longer
/// covers to its parent and what it newly covers to itself — the
/// parent's button bar is repainted only on that Expose. Measured on
/// Xorg 21.1 by tools/vng-scenarios/child-clip-probe.c: the shape
/// (0,0 170x60) of V at (10,10) becoming (0,30 170x60) exposes
/// P 10,10 170x30 and V 0,60 170x30.
#[test]
fn bounding_shape_change_exposes_what_it_uncovers_and_covers() {
    use yserver_protocol::x11::{shape as x11shape, xfixes::RegionRect};
    let mut state = ServerState::new();
    let mut peer = install_client(&mut state, 1);
    let mut backend = RecordingBackend::new();
    let (p, v) = (ResourceId(0x1c0_0001), ResourceId(0x1c0_0002));
    seed_window(&mut state, p, ROOT_WINDOW, 190, 100);
    seed_window(&mut state, v, p, 170, 120);
    for w in [p, v] {
        state.resources.window_mut(w).unwrap().map_state = crate::resources::MapState::Viewable;
    }
    let vw = state.resources.window_mut(v).unwrap();
    (vw.x, vw.y) = (10, 10);
    crate::nested::set_shape_rects(
        &mut state,
        v,
        x11shape::KIND_BOUNDING,
        vec![RegionRect {
            x: 0,
            y: 0,
            width: 170,
            height: 60,
        }],
    );
    let client = state.clients.get_mut(&1).unwrap();
    client.event_masks.insert(p, 0x0000_8000);
    client.event_masks.insert(v, 0x0000_8000);

    // RECTANGLES body: op kind ordering pad dest(4) x_off(2) y_off(2) rects.
    let mut body = vec![x11shape::OP_SET, x11shape::KIND_BOUNDING, 0, 0];
    body.extend_from_slice(&v.0.to_le_bytes());
    body.extend_from_slice(&[0; 4]);
    for field in [0u16, 30, 170, 60] {
        body.extend_from_slice(&field.to_le_bytes());
    }
    let header = yserver_protocol::x11::RequestHeader {
        opcode: 129,
        data: x11shape::RECTANGLES,
        length_units: 6,
    };
    handle_shape_request(
        &mut state,
        &mut backend,
        None,
        ClientId(1),
        SequenceNumber(1),
        header,
        &body,
    )
    .expect("ShapeRectangles");

    let exposes: Vec<(u32, [u16; 4])> = read_all_available(&mut peer)
        .chunks_exact(32)
        .filter(|e| e[0] & 0x7f == 12)
        .map(|e| {
            let at = |i: usize| u16::from_le_bytes([e[i], e[i + 1]]);
            (
                u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
                [at(8), at(10), at(12), at(14)],
            )
        })
        .collect();
    assert_eq!(
        exposes,
        vec![(p.0, [10, 10, 170, 30]), (v.0, [0, 60, 170, 30])]
    );
}
