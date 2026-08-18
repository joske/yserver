//! Private helper-process transport for disposable PRIME route probes.
//!
//! This module deliberately contains no platform/backend orchestration. It
//! owns only the re-exec boundary, inherited-fd contract, whole-route scalar
//! protocol, and watchdog. One request describes everything a later child-side
//! qualifier needs to search the route's candidates and return exactly one
//! resource-free winning plan.

use ::drm::control::{connector, crtc, encoder, plane};

use std::{
    env,
    ffi::OsStr,
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::{net::UnixStream, process::CommandExt},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};
use yserver_core::backend::{CrtcConfigToken, ModeSpec};

use crate::{
    drm::modeset::output_for_exact_probe_assignment,
    kms::{
        render::platform::{
            CopiedQualificationSink, QualifiedScanoutPlan, ScanoutQualificationError,
            qualify_scanout_route_for_worker,
        },
        scanout_route::{RenderDeviceId, RenderKmsRelationship, ScanoutRoute},
        vk::{
            device::VulkanDeviceSelector,
            scanout::{CopiedScanoutPlan, ScanoutAllocationPlan},
            target::CopiedSourcePlan,
        },
    },
    platform::drm::DrmDeviceKey,
};

const REEXEC_ARG: &str = "--yserver-internal-prime-probe-helper-v1";
const CONTROL_FD: RawFd = 198;
const KMS_FD: RawFd = 199;
const INHERIT_SOURCE_FD_MIN: RawFd = 256;

const PROTOCOL_MAGIC: [u8; 4] = *b"YSPB";
const PROTOCOL_VERSION: u16 = 1;
const REQUEST_KIND: u16 = 1;
const RESPONSE_KIND: u16 = 2;
const HEADER_LEN: usize = 12;
const REQUEST_PAYLOAD_LEN: usize = 136;
const RESPONSE_PAYLOAD_LEN: usize = 64;
const REQUEST_FRAME_LEN: usize = HEADER_LEN + REQUEST_PAYLOAD_LEN;
const RESPONSE_FRAME_LEN: usize = HEADER_LEN + RESPONSE_PAYLOAD_LEN;

const ERROR_KMS_RECONSTRUCTION: u32 = 1;
const ERROR_ROUTE_REJECTED: u32 = 2;
const ERROR_ROUTE_INDETERMINATE: u32 = 3;
const ERROR_ROUTE_DEVICE_LOST: u32 = 4;
const ERROR_KMS_INTERNAL: u32 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeVulkanDeviceSelector {
    pub(crate) device_uuid: [u8; 16],
    pub(crate) driver_uuid: [u8; 16],
}

impl From<VulkanDeviceSelector> for ProbeVulkanDeviceSelector {
    fn from(selector: VulkanDeviceSelector) -> Self {
        let (device_uuid, driver_uuid) = selector.uuid_pair();
        Self {
            device_uuid,
            driver_uuid,
        }
    }
}

impl From<ProbeVulkanDeviceSelector> for VulkanDeviceSelector {
    fn from(selector: ProbeVulkanDeviceSelector) -> Self {
        Self::from_uuid_pair(selector.device_uuid, selector.driver_uuid)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeCopiedSink {
    pub(crate) render_device_id: RenderDeviceId,
    pub(crate) selector: ProbeVulkanDeviceSelector,
}

impl From<CopiedQualificationSink> for ProbeCopiedSink {
    fn from(sink: CopiedQualificationSink) -> Self {
        Self {
            render_device_id: sink.id,
            selector: sink.selector.into(),
        }
    }
}

impl From<ProbeCopiedSink> for CopiedQualificationSink {
    fn from(sink: ProbeCopiedSink) -> Self {
        Self {
            id: sink.render_device_id,
            selector: sink.selector.into(),
        }
    }
}

/// Exact KMS objects the child must rediscover and use for atomic `TEST_ONLY`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeKmsHandles {
    pub(crate) connector: connector::Handle,
    pub(crate) encoder: encoder::Handle,
    pub(crate) crtc: crtc::Handle,
    pub(crate) plane: plane::Handle,
}

/// Resource-free description of one complete route qualification.
///
/// The helper owns candidate discovery and search for this route. The optional
/// copied sink is the only sink it may use after copy-free candidates fail.
/// The response therefore identifies one winning plan, not one attempted plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteProbeRequest {
    pub(crate) token: CrtcConfigToken,
    pub(crate) mode: ModeSpec,
    pub(crate) source_route: ScanoutRoute,
    pub(crate) source_selector: ProbeVulkanDeviceSelector,
    pub(crate) copied_sink: Option<ProbeCopiedSink>,
    pub(crate) kms: ProbeKmsHandles,
    pub(crate) fence_timeout_ns: u64,
}

impl RouteProbeRequest {
    fn encode(self) -> [u8; REQUEST_FRAME_LEN] {
        let mut frame = [0_u8; REQUEST_FRAME_LEN];
        encode_header(&mut frame, REQUEST_KIND, REQUEST_PAYLOAD_LEN);
        let mut cursor = HEADER_LEN;
        put_u64(&mut frame, &mut cursor, self.token.0);
        put_u16(&mut frame, &mut cursor, self.mode.width);
        put_u16(&mut frame, &mut cursor, self.mode.height);
        put_u32(&mut frame, &mut cursor, self.mode.vrefresh);
        let (source_kind, source_major, source_minor) =
            encode_render_device_id(self.source_route.render_device_id);
        put_u16(&mut frame, &mut cursor, source_kind);
        put_u16(
            &mut frame,
            &mut cursor,
            encode_relationship(self.source_route.relationship),
        );
        put_u32(&mut frame, &mut cursor, source_major);
        put_u32(&mut frame, &mut cursor, source_minor);
        put_u32(
            &mut frame,
            &mut cursor,
            self.source_route.kms_device_key.major,
        );
        put_u32(
            &mut frame,
            &mut cursor,
            self.source_route.kms_device_key.minor,
        );
        put_bytes(&mut frame, &mut cursor, &self.source_selector.device_uuid);
        put_bytes(&mut frame, &mut cursor, &self.source_selector.driver_uuid);
        match self.copied_sink {
            Some(sink) => {
                let (kind, major, minor) = encode_render_device_id(sink.render_device_id);
                put_u16(&mut frame, &mut cursor, 1);
                put_u16(&mut frame, &mut cursor, kind);
                put_u32(&mut frame, &mut cursor, major);
                put_u32(&mut frame, &mut cursor, minor);
                put_bytes(&mut frame, &mut cursor, &sink.selector.device_uuid);
                put_bytes(&mut frame, &mut cursor, &sink.selector.driver_uuid);
            }
            None => {
                put_u16(&mut frame, &mut cursor, 0);
                put_u16(&mut frame, &mut cursor, 0);
                put_u32(&mut frame, &mut cursor, 0);
                put_u32(&mut frame, &mut cursor, 0);
                put_bytes(&mut frame, &mut cursor, &[0; 16]);
                put_bytes(&mut frame, &mut cursor, &[0; 16]);
            }
        }
        put_u32(&mut frame, &mut cursor, self.kms.connector.into());
        put_u32(&mut frame, &mut cursor, self.kms.encoder.into());
        put_u32(&mut frame, &mut cursor, self.kms.crtc.into());
        put_u32(&mut frame, &mut cursor, self.kms.plane.into());
        put_u64(&mut frame, &mut cursor, self.fence_timeout_ns);
        debug_assert_eq!(cursor, REQUEST_FRAME_LEN);
        frame
    }

    fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        decode_header(frame, REQUEST_KIND, REQUEST_PAYLOAD_LEN)?;
        let mut cursor = HEADER_LEN;
        let token = CrtcConfigToken(take_u64(frame, &mut cursor)?);
        if token.0 == 0 {
            return Err(ProtocolError::ZeroToken);
        }
        let mode = ModeSpec {
            width: take_u16(frame, &mut cursor)?,
            height: take_u16(frame, &mut cursor)?,
            vrefresh: take_u32(frame, &mut cursor)?,
        };
        if mode.width == 0 || mode.height == 0 || mode.vrefresh == 0 {
            return Err(ProtocolError::InvalidMode(mode));
        }
        let source_kind = take_u16(frame, &mut cursor)?;
        let relationship = decode_relationship(take_u16(frame, &mut cursor)?)?;
        let source_major = take_u32(frame, &mut cursor)?;
        let source_minor = take_u32(frame, &mut cursor)?;
        let render_device_id = decode_render_device_id(source_kind, source_major, source_minor)?;
        let kms_device_key = DrmDeviceKey {
            major: take_u32(frame, &mut cursor)?,
            minor: take_u32(frame, &mut cursor)?,
        };
        let source_selector = ProbeVulkanDeviceSelector {
            device_uuid: take_array(frame, &mut cursor)?,
            driver_uuid: take_array(frame, &mut cursor)?,
        };
        let sink_presence = take_u16(frame, &mut cursor)?;
        let sink_kind = take_u16(frame, &mut cursor)?;
        let sink_major = take_u32(frame, &mut cursor)?;
        let sink_minor = take_u32(frame, &mut cursor)?;
        let sink_selector = ProbeVulkanDeviceSelector {
            device_uuid: take_array(frame, &mut cursor)?,
            driver_uuid: take_array(frame, &mut cursor)?,
        };
        let copied_sink = match sink_presence {
            0 => {
                if sink_kind != 0
                    || sink_major != 0
                    || sink_minor != 0
                    || sink_selector.device_uuid != [0; 16]
                    || sink_selector.driver_uuid != [0; 16]
                {
                    return Err(ProtocolError::NonCanonicalAbsentSink);
                }
                None
            }
            1 => Some(ProbeCopiedSink {
                render_device_id: decode_render_device_id(sink_kind, sink_major, sink_minor)?,
                selector: sink_selector,
            }),
            value => return Err(ProtocolError::UnknownSinkPresence(value)),
        };
        let kms = ProbeKmsHandles {
            connector: decode_handle(take_u32(frame, &mut cursor)?, "connector")?,
            encoder: decode_handle(take_u32(frame, &mut cursor)?, "encoder")?,
            crtc: decode_handle(take_u32(frame, &mut cursor)?, "CRTC")?,
            plane: decode_handle(take_u32(frame, &mut cursor)?, "plane")?,
        };
        let fence_timeout_ns = take_u64(frame, &mut cursor)?;
        if fence_timeout_ns == 0 {
            return Err(ProtocolError::ZeroFenceTimeout);
        }
        debug_assert_eq!(cursor, REQUEST_FRAME_LEN);
        Ok(Self {
            token,
            mode,
            source_route: ScanoutRoute::new(render_device_id, kms_device_key, relationship),
            source_selector,
            copied_sink,
            kms,
            fence_timeout_ns,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
enum ProbeStatus {
    Compatible = 0,
    Rejected = 1,
    Indeterminate = 2,
    InternalError = 3,
}

impl ProbeStatus {
    fn decode(value: u16) -> Result<Self, ProtocolError> {
        match value {
            0 => Ok(Self::Compatible),
            1 => Ok(Self::Rejected),
            2 => Ok(Self::Indeterminate),
            3 => Ok(Self::InternalError),
            _ => Err(ProtocolError::UnknownStatus(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProbeFailure {
    pub(crate) error_code: u32,
    pub(crate) detail_code: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteProbeOutcome {
    Compatible(QualifiedScanoutPlan),
    Rejected(ProbeFailure),
    Indeterminate(ProbeFailure),
    Internal(ProbeFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteProbeResponse {
    pub(crate) token: CrtcConfigToken,
    pub(crate) outcome: RouteProbeOutcome,
    pub(crate) elapsed_ns: u64,
}

impl RouteProbeResponse {
    fn encode(self) -> [u8; RESPONSE_FRAME_LEN] {
        let mut frame = [0_u8; RESPONSE_FRAME_LEN];
        encode_header(&mut frame, RESPONSE_KIND, RESPONSE_PAYLOAD_LEN);
        let mut cursor = HEADER_LEN;
        let encoded = EncodedOutcome::from(self.outcome);
        put_u64(&mut frame, &mut cursor, self.token.0);
        put_u16(&mut frame, &mut cursor, encoded.status as u16);
        put_u16(&mut frame, &mut cursor, encoded.qualified_kind);
        put_u16(&mut frame, &mut cursor, encoded.first_plan_kind);
        put_u16(&mut frame, &mut cursor, encoded.second_plan_kind);
        put_u16(&mut frame, &mut cursor, encoded.sink_kind);
        put_u16(&mut frame, &mut cursor, 0);
        put_u32(&mut frame, &mut cursor, encoded.sink_major);
        put_u32(&mut frame, &mut cursor, encoded.sink_minor);
        put_u32(&mut frame, &mut cursor, encoded.error_code);
        put_u64(&mut frame, &mut cursor, encoded.first_plan_value);
        put_u64(&mut frame, &mut cursor, encoded.second_plan_value);
        put_u64(&mut frame, &mut cursor, encoded.detail_code);
        put_u64(&mut frame, &mut cursor, self.elapsed_ns);
        debug_assert_eq!(cursor, RESPONSE_FRAME_LEN);
        frame
    }

    fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        decode_header(frame, RESPONSE_KIND, RESPONSE_PAYLOAD_LEN)?;
        let mut cursor = HEADER_LEN;
        let token = CrtcConfigToken(take_u64(frame, &mut cursor)?);
        if token.0 == 0 {
            return Err(ProtocolError::ZeroToken);
        }
        let status = ProbeStatus::decode(take_u16(frame, &mut cursor)?)?;
        let qualified_kind = take_u16(frame, &mut cursor)?;
        let first_plan_kind = take_u16(frame, &mut cursor)?;
        let second_plan_kind = take_u16(frame, &mut cursor)?;
        let sink_kind = take_u16(frame, &mut cursor)?;
        let reserved = take_u16(frame, &mut cursor)?;
        if reserved != 0 {
            return Err(ProtocolError::NonZeroReserved(u32::from(reserved)));
        }
        let sink_major = take_u32(frame, &mut cursor)?;
        let sink_minor = take_u32(frame, &mut cursor)?;
        let error_code = take_u32(frame, &mut cursor)?;
        let first_plan_value = take_u64(frame, &mut cursor)?;
        let second_plan_value = take_u64(frame, &mut cursor)?;
        let detail_code = take_u64(frame, &mut cursor)?;
        let elapsed_ns = take_u64(frame, &mut cursor)?;
        debug_assert_eq!(cursor, RESPONSE_FRAME_LEN);
        let outcome = decode_outcome(EncodedOutcome {
            status,
            qualified_kind,
            first_plan_kind,
            second_plan_kind,
            sink_kind,
            sink_major,
            sink_minor,
            error_code,
            first_plan_value,
            second_plan_value,
            detail_code,
        })?;
        Ok(Self {
            token,
            outcome,
            elapsed_ns,
        })
    }
}

#[derive(Clone, Copy)]
struct EncodedOutcome {
    status: ProbeStatus,
    qualified_kind: u16,
    first_plan_kind: u16,
    second_plan_kind: u16,
    sink_kind: u16,
    sink_major: u32,
    sink_minor: u32,
    error_code: u32,
    first_plan_value: u64,
    second_plan_value: u64,
    detail_code: u64,
}

impl From<RouteProbeOutcome> for EncodedOutcome {
    fn from(outcome: RouteProbeOutcome) -> Self {
        let mut encoded = Self {
            status: ProbeStatus::Compatible,
            qualified_kind: 0,
            first_plan_kind: 0,
            second_plan_kind: 0,
            sink_kind: 0,
            sink_major: 0,
            sink_minor: 0,
            error_code: 0,
            first_plan_value: 0,
            second_plan_value: 0,
            detail_code: 0,
        };
        match outcome {
            RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Shared(plan)) => {
                encoded.qualified_kind = 1;
                (encoded.first_plan_kind, encoded.first_plan_value) = encode_allocation_plan(plan);
            }
            RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Copied { sink_id, plan }) => {
                encoded.qualified_kind = 2;
                (encoded.first_plan_kind, encoded.first_plan_value) =
                    encode_copied_source_plan(plan.source);
                (encoded.second_plan_kind, encoded.second_plan_value) =
                    encode_allocation_plan(plan.destination);
                (encoded.sink_kind, encoded.sink_major, encoded.sink_minor) =
                    encode_render_device_id(sink_id);
            }
            RouteProbeOutcome::Rejected(failure) => {
                encoded.status = ProbeStatus::Rejected;
                encoded.error_code = failure.error_code;
                encoded.detail_code = failure.detail_code;
            }
            RouteProbeOutcome::Indeterminate(failure) => {
                encoded.status = ProbeStatus::Indeterminate;
                encoded.error_code = failure.error_code;
                encoded.detail_code = failure.detail_code;
            }
            RouteProbeOutcome::Internal(failure) => {
                encoded.status = ProbeStatus::InternalError;
                encoded.error_code = failure.error_code;
                encoded.detail_code = failure.detail_code;
            }
        }
        encoded
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
enum ProtocolError {
    #[error("probe protocol frame length {actual}, expected {expected}")]
    BadLength { actual: usize, expected: usize },
    #[error("probe protocol magic mismatch")]
    BadMagic,
    #[error("probe protocol version {0} is unsupported")]
    UnsupportedVersion(u16),
    #[error("probe protocol frame kind {actual}, expected {expected}")]
    WrongKind { actual: u16, expected: u16 },
    #[error("probe protocol payload length {actual}, expected {expected}")]
    BadPayloadLength { actual: u32, expected: usize },
    #[error("probe protocol contains non-zero reserved scalar {0}")]
    NonZeroReserved(u32),
    #[error("probe protocol token is zero")]
    ZeroToken,
    #[error("probe protocol status {0} is unknown")]
    UnknownStatus(u16),
    #[error("probe protocol render-device identity kind {0} is unknown")]
    UnknownRenderDeviceKind(u16),
    #[error("probe protocol relationship {0} is unknown")]
    UnknownRelationship(u16),
    #[error("probe protocol copied-sink presence {0} is unknown")]
    UnknownSinkPresence(u16),
    #[error("probe protocol absent copied sink has non-zero identity data")]
    NonCanonicalAbsentSink,
    #[error("probe protocol unverified render identity has non-zero DRM identity")]
    NonCanonicalUnverifiedRenderIdentity,
    #[error("probe protocol mode is invalid: {0:?}")]
    InvalidMode(ModeSpec),
    #[error("probe protocol {0} handle is zero")]
    ZeroKmsHandle(&'static str),
    #[error("probe protocol per-fence timeout is zero")]
    ZeroFenceTimeout,
    #[error("probe protocol qualified-plan kind {0} is unknown")]
    UnknownQualifiedPlanKind(u16),
    #[error("probe protocol allocation-plan kind {0} is unknown")]
    UnknownAllocationPlanKind(u16),
    #[error("probe protocol copied-source-plan kind {0} is unknown")]
    UnknownCopiedSourcePlanKind(u16),
    #[error("probe protocol valueless plan kind {kind} has non-zero value {value}")]
    NonCanonicalPlanValue { kind: u16, value: u64 },
    #[error("probe protocol padded-linear row pitch {0} is invalid")]
    InvalidRowPitch(u64),
    #[error("probe protocol {status:?} outcome carries qualified-plan data")]
    FailureCarriesPlan { status: ProbeStatus },
    #[error("probe protocol compatible outcome carries failure data")]
    CompatibleCarriesFailure,
    #[error("probe protocol compatible shared plan carries copied-only data")]
    SharedCarriesCopiedData,
    #[error("probe protocol compatible copied plan is missing source, destination, or sink data")]
    IncompleteCopiedPlan,
    #[error("probe response token {actual} does not match request token {expected}")]
    MismatchedToken { actual: u64, expected: u64 },
    #[error("probe response selected copied sink {actual:?}, expected {expected:?}")]
    MismatchedCopiedSink {
        actual: RenderDeviceId,
        expected: Option<RenderDeviceId>,
    },
}

fn encode_render_device_id(identity: RenderDeviceId) -> (u16, u32, u32) {
    match identity {
        RenderDeviceId::DrmRender(key) => (1, key.major, key.minor),
        RenderDeviceId::UnverifiedFallback => (2, 0, 0),
    }
}

fn decode_render_device_id(
    kind: u16,
    major: u32,
    minor: u32,
) -> Result<RenderDeviceId, ProtocolError> {
    match kind {
        1 => Ok(RenderDeviceId::DrmRender(DrmDeviceKey { major, minor })),
        2 if major == 0 && minor == 0 => Ok(RenderDeviceId::UnverifiedFallback),
        2 => Err(ProtocolError::NonCanonicalUnverifiedRenderIdentity),
        value => Err(ProtocolError::UnknownRenderDeviceKind(value)),
    }
}

fn encode_relationship(relationship: RenderKmsRelationship) -> u16 {
    match relationship {
        RenderKmsRelationship::Same => 1,
        RenderKmsRelationship::Different => 2,
        RenderKmsRelationship::Unknown => 3,
    }
}

fn decode_relationship(value: u16) -> Result<RenderKmsRelationship, ProtocolError> {
    match value {
        1 => Ok(RenderKmsRelationship::Same),
        2 => Ok(RenderKmsRelationship::Different),
        3 => Ok(RenderKmsRelationship::Unknown),
        value => Err(ProtocolError::UnknownRelationship(value)),
    }
}

fn decode_handle<T>(raw: u32, label: &'static str) -> Result<T, ProtocolError>
where
    T: From<::drm::control::RawResourceHandle>,
{
    ::drm::control::from_u32(raw).ok_or(ProtocolError::ZeroKmsHandle(label))
}

fn encode_allocation_plan(plan: ScanoutAllocationPlan) -> (u16, u64) {
    match plan {
        ScanoutAllocationPlan::GbmModifier(modifier) => (1, modifier),
        ScanoutAllocationPlan::DrmModifier(modifier) => (2, modifier),
        ScanoutAllocationPlan::PaddedExplicitLinear { row_pitch } => (3, u64::from(row_pitch)),
        ScanoutAllocationPlan::ExplicitLinear => (4, 0),
        ScanoutAllocationPlan::LegacyLinear => (5, 0),
    }
}

fn decode_allocation_plan(kind: u16, value: u64) -> Result<ScanoutAllocationPlan, ProtocolError> {
    match kind {
        1 => Ok(ScanoutAllocationPlan::GbmModifier(value)),
        2 => Ok(ScanoutAllocationPlan::DrmModifier(value)),
        3 => {
            let row_pitch = u32::try_from(value)
                .ok()
                .filter(|row_pitch| *row_pitch != 0)
                .ok_or(ProtocolError::InvalidRowPitch(value))?;
            Ok(ScanoutAllocationPlan::PaddedExplicitLinear { row_pitch })
        }
        4 | 5 if value != 0 => Err(ProtocolError::NonCanonicalPlanValue { kind, value }),
        4 => Ok(ScanoutAllocationPlan::ExplicitLinear),
        5 => Ok(ScanoutAllocationPlan::LegacyLinear),
        value => Err(ProtocolError::UnknownAllocationPlanKind(value)),
    }
}

fn encode_copied_source_plan(plan: CopiedSourcePlan) -> (u16, u64) {
    match plan {
        CopiedSourcePlan::DrmModifier(modifier) => (1, modifier),
    }
}

fn decode_copied_source_plan(kind: u16, value: u64) -> Result<CopiedSourcePlan, ProtocolError> {
    match kind {
        1 => Ok(CopiedSourcePlan::DrmModifier(value)),
        value => Err(ProtocolError::UnknownCopiedSourcePlanKind(value)),
    }
}

fn decode_outcome(encoded: EncodedOutcome) -> Result<RouteProbeOutcome, ProtocolError> {
    match encoded.status {
        ProbeStatus::Compatible => {
            if encoded.error_code != 0 || encoded.detail_code != 0 {
                return Err(ProtocolError::CompatibleCarriesFailure);
            }
            match encoded.qualified_kind {
                1 => {
                    if encoded.second_plan_kind != 0
                        || encoded.second_plan_value != 0
                        || encoded.sink_kind != 0
                        || encoded.sink_major != 0
                        || encoded.sink_minor != 0
                    {
                        return Err(ProtocolError::SharedCarriesCopiedData);
                    }
                    Ok(RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Shared(
                        decode_allocation_plan(encoded.first_plan_kind, encoded.first_plan_value)?,
                    )))
                }
                2 => {
                    if encoded.first_plan_kind == 0
                        || encoded.second_plan_kind == 0
                        || encoded.sink_kind == 0
                    {
                        return Err(ProtocolError::IncompleteCopiedPlan);
                    }
                    let sink_id = decode_render_device_id(
                        encoded.sink_kind,
                        encoded.sink_major,
                        encoded.sink_minor,
                    )?;
                    Ok(RouteProbeOutcome::Compatible(
                        QualifiedScanoutPlan::Copied {
                            sink_id,
                            plan: CopiedScanoutPlan {
                                source: decode_copied_source_plan(
                                    encoded.first_plan_kind,
                                    encoded.first_plan_value,
                                )?,
                                destination: decode_allocation_plan(
                                    encoded.second_plan_kind,
                                    encoded.second_plan_value,
                                )?,
                            },
                        },
                    ))
                }
                value => Err(ProtocolError::UnknownQualifiedPlanKind(value)),
            }
        }
        status => {
            if encoded.qualified_kind != 0
                || encoded.first_plan_kind != 0
                || encoded.second_plan_kind != 0
                || encoded.sink_kind != 0
                || encoded.sink_major != 0
                || encoded.sink_minor != 0
                || encoded.first_plan_value != 0
                || encoded.second_plan_value != 0
            {
                return Err(ProtocolError::FailureCarriesPlan { status });
            }
            let failure = ProbeFailure {
                error_code: encoded.error_code,
                detail_code: encoded.detail_code,
            };
            Ok(match status {
                ProbeStatus::Rejected => RouteProbeOutcome::Rejected(failure),
                ProbeStatus::Indeterminate => RouteProbeOutcome::Indeterminate(failure),
                ProbeStatus::InternalError => RouteProbeOutcome::Internal(failure),
                ProbeStatus::Compatible => unreachable!(),
            })
        }
    }
}

fn validate_response_for_request(
    request: &RouteProbeRequest,
    response: &RouteProbeResponse,
) -> Result<(), ProtocolError> {
    if response.token != request.token {
        return Err(ProtocolError::MismatchedToken {
            actual: response.token.0,
            expected: request.token.0,
        });
    }
    if let RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Copied { sink_id, .. }) =
        response.outcome
    {
        let expected = request.copied_sink.map(|sink| sink.render_device_id);
        if expected != Some(sink_id) {
            return Err(ProtocolError::MismatchedCopiedSink {
                actual: sink_id,
                expected,
            });
        }
    }
    Ok(())
}

/// Resolve the exact executable for one production process-isolated helper
/// invocation.
fn probe_helper_executable() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // Resolve this at exec time through procfs. Unlike the pathname from
        // `current_exe`, `/proc/self/exe` continues to name the exact running
        // image after an atomic deployment replaces or unlinks that pathname,
        // so parent and helper cannot silently cross protocol versions.
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        env::current_exe()
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn arm_helper_parent_death_signal(expected_parent: libc::pid_t) -> io::Result<()> {
    // SIGKILL needs no userspace handler and remains effective across exec.
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_PDEATHSIG stores one signal number in the calling task.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } < 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut signal = libc::SIGKILL;
        // SAFETY: PROC_PDEATHSIG_CTL reads one c_int from `data` and applies it
        // to the calling process selected by (P_PID, 0).
        if unsafe {
            libc::procctl(
                libc::P_PID,
                0,
                libc::PROC_PDEATHSIG_CTL,
                std::ptr::from_mut(&mut signal).cast(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    // `getppid` closes the fork-to-parent-death-arm race: if the captured
    // yserver parent already exited, pre-exec fails instead of launching an
    // orphan helper without a future parent-death notification.
    if unsafe { libc::getppid() } != expected_parent {
        // Keep this pre-exec error allocation-free.
        return Err(io::Error::from_raw_os_error(libc::ECHILD));
    }
    Ok(())
}

#[allow(dead_code)]
pub(crate) struct ProbeHelperSupervisor {
    executable: PathBuf,
    watchdog: Duration,
}

/// Failure stage for one supervised helper invocation.
///
/// `NotStarted` proves that no child handle was created, so no helper-side
/// Vulkan/KMS work could have begun. `ChildStartedUncertain` means a child did
/// exist and its cleanup/resource state cannot be inferred from a failed IPC
/// exchange.
#[derive(Debug)]
pub(crate) enum ProbeHelperRunError {
    NotStarted(io::Error),
    ChildStartedUncertain(io::Error),
}

impl ProbeHelperRunError {
    pub(crate) fn kind(&self) -> io::ErrorKind {
        match self {
            Self::NotStarted(error) | Self::ChildStartedUncertain(error) => error.kind(),
        }
    }
}

impl std::fmt::Display for ProbeHelperRunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStarted(error) => {
                write!(formatter, "probe helper did not start: {error}")
            }
            Self::ChildStartedUncertain(error) => {
                write!(formatter, "probe helper child state is uncertain: {error}")
            }
        }
    }
}

impl std::error::Error for ProbeHelperRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NotStarted(error) | Self::ChildStartedUncertain(error) => Some(error),
        }
    }
}

#[allow(dead_code)]
impl ProbeHelperSupervisor {
    pub(crate) fn for_current_exe(watchdog: Duration) -> io::Result<Self> {
        Ok(Self {
            executable: probe_helper_executable()?,
            watchdog,
        })
    }

    pub(crate) fn run(
        &self,
        kms_fd: BorrowedFd<'_>,
        request: RouteProbeRequest,
    ) -> Result<RouteProbeResponse, ProbeHelperRunError> {
        let (parent_control, child_control) =
            UnixStream::pair().map_err(ProbeHelperRunError::NotStarted)?;
        let inherited_control = duplicate_fd_at_least(child_control.as_fd())
            .map_err(ProbeHelperRunError::NotStarted)?;
        let inherited_kms =
            duplicate_fd_at_least(kms_fd).map_err(ProbeHelperRunError::NotStarted)?;
        let control_source = inherited_control.as_raw_fd();
        let kms_source = inherited_kms.as_raw_fd();
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        // SAFETY: getpid has no preconditions and returns the process ID that
        // the forked helper must still observe as its parent after arming
        // PR_SET_PDEATHSIG.
        let supervisor_pid = unsafe { libc::getpid() };

        let mut command = Command::new(&self.executable);
        command
            .arg(REEXEC_ARG)
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        // SAFETY: after fork and before exec, the closure performs only
        // async-signal-safe parent-death/getppid/dup2/fcntl syscalls and
        // constructs allocation-free OS errors. Source fds are duplicated
        // above both fixed targets, so the first dup2 cannot invalidate the
        // second source.
        unsafe {
            command.pre_exec(move || {
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                arm_helper_parent_death_signal(supervisor_pid)?;
                duplicate_to_inherited_slot(control_source, CONTROL_FD)?;
                duplicate_to_inherited_slot(kms_source, KMS_FD)?;
                Ok(())
            });
        }
        let child = command.spawn().map_err(ProbeHelperRunError::NotStarted)?;
        drop(child_control);
        drop(inherited_control);
        drop(inherited_kms);
        supervise_exchange(child, parent_control, request, self.watchdog)
            .map_err(ProbeHelperRunError::ChildStartedUncertain)
    }
}

/// Called by the `yserver` binary before normal argument parsing.
///
/// `None` means this is an ordinary server invocation. `Some` means the exact
/// private re-exec marker was present and the caller must exit after returning
/// this result.
#[doc(hidden)]
pub fn run_reexec_helper_if_requested() -> Option<io::Result<()>> {
    let mut args = env::args_os();
    let _executable = args.next();
    if args.next().as_deref() != Some(OsStr::new(REEXEC_ARG)) {
        return None;
    }
    if args.next().is_some() {
        return Some(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal PRIME probe helper accepts no additional arguments",
        )));
    }
    Some(run_reexec_helper())
}

fn run_reexec_helper() -> io::Result<()> {
    let control = take_inherited_fd(CONTROL_FD, "control socket")?;
    let kms = take_inherited_fd(KMS_FD, "KMS device")?;
    let control = UnixStream::from(control);
    let device = crate::drm::Device::from_inherited_kms_fd(kms, "<inherited probe KMS fd>");
    serve_one(control, device, qualify_route_probe)
}

fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn probe_failure(error_code: u32, error: &io::Error) -> ProbeFailure {
    ProbeFailure {
        error_code,
        detail_code: error
            .raw_os_error()
            .map_or(0, |errno| u64::from(errno.unsigned_abs())),
    }
}

fn route_probe_outcome(
    result: Result<QualifiedScanoutPlan, ScanoutQualificationError>,
) -> RouteProbeOutcome {
    match result {
        Ok(qualified) => RouteProbeOutcome::Compatible(qualified),
        Err(ScanoutQualificationError::Rejected(error)) => {
            RouteProbeOutcome::Rejected(probe_failure(ERROR_ROUTE_REJECTED, &error))
        }
        Err(ScanoutQualificationError::Indeterminate(error)) => {
            RouteProbeOutcome::Indeterminate(probe_failure(ERROR_ROUTE_INDETERMINATE, &error))
        }
        Err(ScanoutQualificationError::DeviceLost(error)) => {
            RouteProbeOutcome::Indeterminate(probe_failure(ERROR_ROUTE_DEVICE_LOST, &error))
        }
    }
}

fn kms_reconstruction_outcome(error: &io::Error) -> RouteProbeOutcome {
    if matches!(
        error.kind(),
        io::ErrorKind::InvalidInput
            | io::ErrorKind::NotFound
            | io::ErrorKind::NotConnected
            | io::ErrorKind::Unsupported
    ) {
        RouteProbeOutcome::Rejected(probe_failure(ERROR_KMS_RECONSTRUCTION, error))
    } else {
        // A hard inherited-fd/ioctl failure is not evidence that another
        // representation is incompatible. Keep it distinct from ordinary
        // rejection so the parent does not continue after uncertain KMS state.
        RouteProbeOutcome::Internal(probe_failure(ERROR_KMS_INTERNAL, error))
    }
}

fn qualify_route_probe(
    device: Rc<crate::drm::Device>,
    request: RouteProbeRequest,
) -> RouteProbeResponse {
    let started = Instant::now();
    let output = match output_for_exact_probe_assignment(
        &device,
        request.kms.connector,
        request.kms.encoder,
        request.kms.crtc,
        request.kms.plane,
        request.mode,
    ) {
        Ok(output) => output,
        Err(error) => {
            let outcome = kms_reconstruction_outcome(&error);
            match outcome {
                RouteProbeOutcome::Rejected(_) => log::warn!(
                    "isolated PRIME probe token {:?} rejected exact KMS assignment {:?} for route \
                     {:?}: {error}",
                    request.token,
                    request.kms,
                    request.source_route,
                ),
                RouteProbeOutcome::Internal(_) => log::error!(
                    "isolated PRIME probe token {:?} failed to reconstruct KMS assignment {:?} \
                     for route {:?}: {error}",
                    request.token,
                    request.kms,
                    request.source_route,
                ),
                RouteProbeOutcome::Compatible(_) | RouteProbeOutcome::Indeterminate(_) => {
                    unreachable!("KMS reconstruction maps only to rejection or internal failure")
                }
            }
            return RouteProbeResponse {
                token: request.token,
                outcome,
                elapsed_ns: elapsed_ns(started),
            };
        }
    };

    let result = qualify_scanout_route_for_worker(
        request.source_selector.into(),
        request.copied_sink.map(Into::into),
        device,
        &output,
        request.source_route,
        u32::from(request.mode.width),
        u32::from(request.mode.height),
        request.fence_timeout_ns,
    );
    match &result {
        Ok(qualified) => {
            log::info!(
                "isolated PRIME probe token {:?} qualified {qualified:?} for route {:?} in {} ms",
                request.token,
                request.source_route,
                started.elapsed().as_millis(),
            );
        }
        Err(ScanoutQualificationError::Rejected(error)) => {
            log::info!(
                "isolated PRIME probe token {:?} rejected route {:?} after {} ms: {error}",
                request.token,
                request.source_route,
                started.elapsed().as_millis(),
            );
        }
        Err(ScanoutQualificationError::Indeterminate(error)) => {
            log::error!(
                "isolated PRIME probe token {:?} became indeterminate for route {:?} after {} ms: \
                 {error}",
                request.token,
                request.source_route,
                started.elapsed().as_millis(),
            );
        }
        Err(ScanoutQualificationError::DeviceLost(error)) => {
            log::error!(
                "isolated PRIME probe token {:?} lost a Vulkan device for route {:?} after {} ms: \
                 {error}",
                request.token,
                request.source_route,
                started.elapsed().as_millis(),
            );
        }
    }
    let outcome = route_probe_outcome(result);
    RouteProbeResponse {
        token: request.token,
        outcome,
        elapsed_ns: elapsed_ns(started),
    }
}

fn serve_one<H>(mut control: UnixStream, device: crate::drm::Device, handler: H) -> io::Result<()>
where
    H: FnOnce(Rc<crate::drm::Device>, RouteProbeRequest) -> RouteProbeResponse,
{
    let mut request_frame = [0_u8; REQUEST_FRAME_LEN];
    control.read_exact(&mut request_frame)?;
    let request = RouteProbeRequest::decode(&request_frame).map_err(protocol_io_error)?;
    let response = handler(Rc::new(device), request);
    validate_response_for_request(&request, &response).map_err(protocol_io_error)?;
    control.write_all(&response.encode())?;
    Ok(())
}

fn supervise_exchange(
    child: Child,
    mut control: UnixStream,
    request: RouteProbeRequest,
    watchdog: Duration,
) -> io::Result<RouteProbeResponse> {
    let result = (|| {
        // Keep all fallible post-spawn setup inside this closure. The cleanup
        // below therefore runs for socket setup, deadline construction,
        // protocol I/O, decode, and validation failures alike.
        control.set_nonblocking(true)?;
        let deadline = Instant::now().checked_add(watchdog).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "probe watchdog overflow")
        })?;
        write_all_until(&mut control, &request.encode(), deadline)?;
        let mut response_frame = [0_u8; RESPONSE_FRAME_LEN];
        read_exact_until(&mut control, &mut response_frame, deadline)?;
        let response = RouteProbeResponse::decode(&response_frame).map_err(protocol_io_error)?;
        validate_response_for_request(&request, &response).map_err(protocol_io_error)?;
        Ok(response)
    })();

    // A valid response is the end of the protocol. Do not synchronously wait
    // for helper-side Vulkan destructors: that would reintroduce the exact
    // driver-stall failure this process boundary is meant to contain.
    terminate_and_reap_async(child);
    result
}

fn terminate_and_reap_async(mut child: Child) {
    let _ = child.kill();
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    if let Err(error) = thread::Builder::new()
        .name("yserver-probe-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
    {
        log::warn!("failed to spawn disposable-probe child reaper: {error}");
    }
}

fn duplicate_fd_at_least(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    // SAFETY: fcntl does not take ownership of `fd`; on success it returns a new
    // close-on-exec descriptor owned by the caller.
    let duplicated =
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, INHERIT_SOURCE_FD_MIN) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful F_DUPFD_CLOEXEC returns one fresh owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn duplicate_to_inherited_slot(source: RawFd, target: RawFd) -> io::Result<()> {
    // SAFETY: both are integer fd slots in the forked child. dup2 atomically
    // replaces `target` and leaves `source` open.
    if unsafe { libc::dup2(source, target) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // dup2 normally clears CLOEXEC, but clear it explicitly so the contract also
    // holds if a platform ever returns source == target.
    if unsafe { libc::fcntl(target, libc::F_SETFD, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn take_inherited_fd(fd: RawFd, label: &str) -> io::Result<OwnedFd> {
    // SAFETY: F_GETFD only validates the descriptor and does not alter it.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        let source = io::Error::last_os_error();
        return Err(io::Error::new(
            source.kind(),
            format!("internal PRIME probe helper missing inherited {label} fd {fd}: {source}"),
        ));
    }
    // SAFETY: the hidden re-exec contract transfers unique ownership of this
    // fixed descriptor to the helper process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn write_all_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_fd(stream.as_raw_fd(), libc::POLLOUT, deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn read_exact_until(
    stream: &mut UnixStream,
    mut bytes: &mut [u8],
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        match stream.read(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(read) => {
                let (_, remaining) = bytes.split_at_mut(read);
                bytes = remaining;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_fd(stream.as_raw_fd(), libc::POLLIN, deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn wait_fd(fd: RawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "disposable probe helper watchdog expired",
            ));
        }
        let remaining = deadline.duration_since(now);
        let timeout_ms = remaining
            .as_millis()
            .saturating_add(u128::from(
                !remaining.subsec_nanos().is_multiple_of(1_000_000),
            ))
            .clamp(1, i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: `poll_fd` points to one initialized entry for the call.
        let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd), 1, timeout_ms) };
        if ready == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "disposable probe helper watchdog expired",
            ));
        }
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll_fd.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "disposable probe helper control fd became invalid",
            ));
        }
        if poll_fd.revents & (events | libc::POLLERR | libc::POLLHUP) != 0 {
            return Ok(());
        }
    }
}

fn encode_header(frame: &mut [u8], kind: u16, payload_len: usize) {
    frame[..4].copy_from_slice(&PROTOCOL_MAGIC);
    frame[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    frame[6..8].copy_from_slice(&kind.to_le_bytes());
    frame[8..12].copy_from_slice(&(payload_len as u32).to_le_bytes());
}

fn decode_header(
    frame: &[u8],
    expected_kind: u16,
    expected_payload_len: usize,
) -> Result<(), ProtocolError> {
    let expected_len = HEADER_LEN + expected_payload_len;
    if frame.len() != expected_len {
        return Err(ProtocolError::BadLength {
            actual: frame.len(),
            expected: expected_len,
        });
    }
    if frame[..4] != PROTOCOL_MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    let version = u16::from_le_bytes([frame[4], frame[5]]);
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let kind = u16::from_le_bytes([frame[6], frame[7]]);
    if kind != expected_kind {
        return Err(ProtocolError::WrongKind {
            actual: kind,
            expected: expected_kind,
        });
    }
    let payload_len = u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]);
    if payload_len as usize != expected_payload_len {
        return Err(ProtocolError::BadPayloadLength {
            actual: payload_len,
            expected: expected_payload_len,
        });
    }
    Ok(())
}

fn put_u16(frame: &mut [u8], cursor: &mut usize, value: u16) {
    frame[*cursor..*cursor + 2].copy_from_slice(&value.to_le_bytes());
    *cursor += 2;
}

fn put_u32(frame: &mut [u8], cursor: &mut usize, value: u32) {
    frame[*cursor..*cursor + 4].copy_from_slice(&value.to_le_bytes());
    *cursor += 4;
}

fn put_u64(frame: &mut [u8], cursor: &mut usize, value: u64) {
    frame[*cursor..*cursor + 8].copy_from_slice(&value.to_le_bytes());
    *cursor += 8;
}

fn put_bytes(frame: &mut [u8], cursor: &mut usize, value: &[u8]) {
    frame[*cursor..*cursor + value.len()].copy_from_slice(value);
    *cursor += value.len();
}

fn take_u16(frame: &[u8], cursor: &mut usize) -> Result<u16, ProtocolError> {
    let end = cursor.saturating_add(2);
    let bytes = frame.get(*cursor..end).ok_or(ProtocolError::BadLength {
        actual: frame.len(),
        expected: end,
    })?;
    *cursor = end;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn take_u32(frame: &[u8], cursor: &mut usize) -> Result<u32, ProtocolError> {
    let end = cursor.saturating_add(4);
    let bytes = frame.get(*cursor..end).ok_or(ProtocolError::BadLength {
        actual: frame.len(),
        expected: end,
    })?;
    *cursor = end;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn take_u64(frame: &[u8], cursor: &mut usize) -> Result<u64, ProtocolError> {
    let end = cursor.saturating_add(8);
    let bytes = frame.get(*cursor..end).ok_or(ProtocolError::BadLength {
        actual: frame.len(),
        expected: end,
    })?;
    *cursor = end;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn take_array<const N: usize>(frame: &[u8], cursor: &mut usize) -> Result<[u8; N], ProtocolError> {
    let end = cursor.saturating_add(N);
    let bytes = frame.get(*cursor..end).ok_or(ProtocolError::BadLength {
        actual: frame.len(),
        expected: end,
    })?;
    *cursor = end;
    Ok(bytes.try_into().expect("slice length was checked"))
}

fn protocol_io_error(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs::File, os::fd::AsRawFd, sync::mpsc};

    #[cfg(target_os = "linux")]
    const PDEATHSIG_NESTED_ENV: &str = "YSERVER_TEST_PDEATHSIG_NESTED_PARENT";
    #[cfg(target_os = "linux")]
    const PDEATHSIG_PID_MARKER: &str = "YSERVER_TEST_PDEATHSIG_CHILD_PID=";

    fn drm_render(major: u32, minor: u32) -> RenderDeviceId {
        RenderDeviceId::DrmRender(DrmDeviceKey { major, minor })
    }

    fn selector(seed: u8) -> ProbeVulkanDeviceSelector {
        ProbeVulkanDeviceSelector {
            device_uuid: [seed; 16],
            driver_uuid: [seed.wrapping_add(0x40); 16],
        }
    }

    fn handle<T>(raw: u32) -> T
    where
        T: From<::drm::control::RawResourceHandle>,
    {
        ::drm::control::from_u32(raw).expect("non-zero test handle")
    }

    fn request(token: u64, with_sink: bool) -> RouteProbeRequest {
        RouteProbeRequest {
            token: CrtcConfigToken(token),
            mode: ModeSpec {
                width: 1600,
                height: 1200,
                vrefresh: 42,
            },
            source_route: ScanoutRoute::new(
                drm_render(226, 128),
                DrmDeviceKey {
                    major: 226,
                    minor: 1,
                },
                RenderKmsRelationship::Different,
            ),
            source_selector: selector(0x11),
            copied_sink: with_sink.then_some(ProbeCopiedSink {
                render_device_id: drm_render(226, 129),
                selector: selector(0x22),
            }),
            kms: ProbeKmsHandles {
                connector: handle(17),
                encoder: handle(18),
                crtc: handle(19),
                plane: handle(20),
            },
            fence_timeout_ns: 200_000_000,
        }
    }

    #[test]
    fn wire_selectors_round_trip_platform_uuid_pairs_and_copied_sink_identity() {
        let wire_selector = selector(0x23);
        let platform_selector: VulkanDeviceSelector = wire_selector.into();
        assert_eq!(
            ProbeVulkanDeviceSelector::from(platform_selector),
            wire_selector
        );

        let wire_sink = ProbeCopiedSink {
            render_device_id: drm_render(226, 131),
            selector: wire_selector,
        };
        let platform_sink: CopiedQualificationSink = wire_sink.into();
        assert_eq!(ProbeCopiedSink::from(platform_sink), wire_sink);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn helper_reexec_uses_the_running_procfs_image() {
        assert_eq!(
            probe_helper_executable().expect("Linux helper executable"),
            PathBuf::from("/proc/self/exe")
        );
    }

    #[test]
    fn supervisor_classifies_spawn_failure_as_not_started() {
        let supervisor = ProbeHelperSupervisor {
            executable: PathBuf::from("/definitely/missing/yserver-probe-helper"),
            watchdog: Duration::from_secs(1),
        };
        let kms = File::open("/dev/null").expect("open inert KMS fixture");
        let error = supervisor
            .run(kms.as_fd(), request(21, true))
            .expect_err("missing executable must fail before a child starts");

        assert!(matches!(&error, ProbeHelperRunError::NotStarted(_)));
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("did not start"));
    }

    #[test]
    fn supervisor_classifies_post_spawn_exchange_failure_as_uncertain() {
        let supervisor = ProbeHelperSupervisor {
            executable: PathBuf::from("/bin/true"),
            watchdog: Duration::from_secs(1),
        };
        let kms = File::open("/dev/null").expect("open inert KMS fixture");
        let error = supervisor
            .run(kms.as_fd(), request(22, true))
            .expect_err("non-helper child must fail its protocol exchange");

        assert!(matches!(
            &error,
            ProbeHelperRunError::ChildStartedUncertain(_)
        ));
        assert!(matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
        ));
        assert!(error.to_string().contains("state is uncertain"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    // Deliberately exit this nested process without waiting: the outer test is
    // verifying that PDEATHSIG, rather than normal Child cleanup, kills it.
    #[allow(clippy::zombie_processes)]
    fn pdeathsig_nested_parent() {
        if env::var_os(PDEATHSIG_NESTED_ENV).is_none() {
            return;
        }

        // SAFETY: getpid has no preconditions and is called before spawning
        // the child whose parent identity it anchors.
        let expected_parent = unsafe { libc::getpid() };
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: this invokes the same allocation-free syscall helper as the
        // production pre-exec path.
        unsafe {
            command.pre_exec(move || arm_helper_parent_death_signal(expected_parent));
        }
        let child = command.spawn().expect("spawn nested pdeathsig child");
        println!("{PDEATHSIG_PID_MARKER}{}", child.id());
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn pdeathsig_parent_race_check_rejects_a_stale_parent() {
        let mut command = Command::new("/bin/true");
        // SAFETY: the production helper performs only the documented prctl and
        // getppid syscalls; -1 cannot equal a real parent PID.
        unsafe {
            command.pre_exec(|| arm_helper_parent_death_signal(-1));
        }
        let error = command
            .spawn()
            .expect_err("stale captured parent must fail before exec");
        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pdeathsig_kills_a_running_helper_when_its_parent_exits() {
        let output = Command::new(env::current_exe().expect("current test executable"))
            .arg("--exact")
            .arg("internal_probe::tests::pdeathsig_nested_parent")
            .arg("--nocapture")
            .env(PDEATHSIG_NESTED_ENV, "1")
            .output()
            .expect("run nested parent test process");
        assert!(
            output.status.success(),
            "nested parent failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let child_pid = stdout
            .split_whitespace()
            .find_map(|word| word.strip_prefix(PDEATHSIG_PID_MARKER))
            .and_then(|raw| raw.parse::<libc::pid_t>().ok())
            .expect("nested parent reported child PID");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = std::fs::read_to_string(format!("/proc/{child_pid}/stat"));
            let running = match stat {
                Ok(stat) => stat
                    .rsplit_once(") ")
                    .and_then(|(_, fields)| fields.as_bytes().first().copied())
                    .is_some_and(|state| !matches!(state, b'Z' | b'X')),
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => panic!("read nested child state: {error}"),
            };
            if !running {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "helper {child_pid} survived after its parent exited"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn qualification_errors_preserve_rejected_indeterminate_and_device_lost_policy() {
        let rejected = route_probe_outcome(Err(ScanoutQualificationError::Rejected(
            io::Error::from_raw_os_error(libc::EINVAL),
        )));
        assert_eq!(
            rejected,
            RouteProbeOutcome::Rejected(ProbeFailure {
                error_code: ERROR_ROUTE_REJECTED,
                detail_code: libc::EINVAL as u64,
            })
        );

        let indeterminate = route_probe_outcome(Err(ScanoutQualificationError::Indeterminate(
            io::Error::new(io::ErrorKind::TimedOut, "fence timeout"),
        )));
        assert!(matches!(
            indeterminate,
            RouteProbeOutcome::Indeterminate(ProbeFailure {
                error_code: ERROR_ROUTE_INDETERMINATE,
                ..
            })
        ));

        let device_lost = route_probe_outcome(Err(ScanoutQualificationError::DeviceLost(
            io::Error::other("VK_ERROR_DEVICE_LOST"),
        )));
        assert!(matches!(
            device_lost,
            RouteProbeOutcome::Indeterminate(ProbeFailure {
                error_code: ERROR_ROUTE_DEVICE_LOST,
                ..
            })
        ));
    }

    #[test]
    fn kms_reconstruction_distinguishes_stale_assignment_from_hard_io_failure() {
        assert!(matches!(
            kms_reconstruction_outcome(&io::Error::new(io::ErrorKind::InvalidInput, "stale CRTC")),
            RouteProbeOutcome::Rejected(ProbeFailure {
                error_code: ERROR_KMS_RECONSTRUCTION,
                ..
            })
        ));
        assert!(matches!(
            kms_reconstruction_outcome(&io::Error::from_raw_os_error(libc::EIO)),
            RouteProbeOutcome::Internal(ProbeFailure {
                error_code: ERROR_KMS_INTERNAL,
                ..
            })
        ));
    }

    fn copied_response(request: &RouteProbeRequest) -> RouteProbeResponse {
        RouteProbeResponse {
            token: request.token,
            outcome: RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Copied {
                sink_id: request
                    .copied_sink
                    .expect("copied response needs a sink")
                    .render_device_id,
                plan: CopiedScanoutPlan {
                    source: CopiedSourcePlan::DrmModifier(0),
                    destination: ScanoutAllocationPlan::GbmModifier(0x0300_0000_0060_6015),
                },
            }),
            elapsed_ns: 17_000_000,
        }
    }

    #[test]
    fn whole_route_request_round_trips_with_and_without_copied_sink() {
        let copied = request(0x1122_3344_5566_7788, true);
        let copied_frame = copied.encode();
        assert_eq!(
            &copied_frame[HEADER_LEN..HEADER_LEN + 8],
            &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]
        );
        assert_eq!(
            &copied_frame[HEADER_LEN + 8..HEADER_LEN + 12],
            &[0x40, 0x06, 0xb0, 0x04]
        );

        for request in [copied, request(0x8877_6655_4433_2211, false)] {
            assert_eq!(RouteProbeRequest::decode(&request.encode()), Ok(request));
        }

        let mut request_frame = request(7, true).encode();

        request_frame[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        assert_eq!(
            RouteProbeRequest::decode(&request_frame),
            Err(ProtocolError::UnsupportedVersion(PROTOCOL_VERSION + 1))
        );
    }

    #[test]
    fn response_round_trips_all_outcomes_and_allocation_plans() {
        let token = CrtcConfigToken(91);
        let mut responses = vec![
            ScanoutAllocationPlan::GbmModifier(0x0300_0000_0060_6015),
            ScanoutAllocationPlan::DrmModifier(0x0200_0000_0000_0008),
            ScanoutAllocationPlan::PaddedExplicitLinear { row_pitch: 8192 },
            ScanoutAllocationPlan::ExplicitLinear,
            ScanoutAllocationPlan::LegacyLinear,
        ]
        .into_iter()
        .map(|plan| RouteProbeResponse {
            token,
            outcome: RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Shared(plan)),
            elapsed_ns: 1,
        })
        .collect::<Vec<_>>();
        responses.extend([
            RouteProbeResponse {
                token,
                outcome: RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Copied {
                    sink_id: RenderDeviceId::UnverifiedFallback,
                    plan: CopiedScanoutPlan {
                        source: CopiedSourcePlan::DrmModifier(0),
                        destination: ScanoutAllocationPlan::LegacyLinear,
                    },
                }),
                elapsed_ns: 2,
            },
            RouteProbeResponse {
                token,
                outcome: RouteProbeOutcome::Rejected(ProbeFailure {
                    error_code: 5,
                    detail_code: 6,
                }),
                elapsed_ns: 3,
            },
            RouteProbeResponse {
                token,
                outcome: RouteProbeOutcome::Indeterminate(ProbeFailure {
                    error_code: 7,
                    detail_code: 8,
                }),
                elapsed_ns: 4,
            },
            RouteProbeResponse {
                token,
                outcome: RouteProbeOutcome::Internal(ProbeFailure {
                    error_code: 9,
                    detail_code: 10,
                }),
                elapsed_ns: 5,
            },
        ]);

        for response in responses {
            assert_eq!(RouteProbeResponse::decode(&response.encode()), Ok(response));
        }
    }

    #[test]
    fn request_codec_rejects_malformed_route_fields() {
        let valid_request = request(7, true);
        let frame = valid_request.encode();
        assert!(matches!(
            RouteProbeRequest::decode(&frame[..frame.len() - 1]),
            Err(ProtocolError::BadLength { .. })
        ));

        let mut zero_token = frame;
        zero_token[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&0_u64.to_le_bytes());
        assert_eq!(
            RouteProbeRequest::decode(&zero_token),
            Err(ProtocolError::ZeroToken)
        );

        let mut invalid_mode = frame;
        invalid_mode[HEADER_LEN + 8..HEADER_LEN + 10].copy_from_slice(&0_u16.to_le_bytes());
        assert!(matches!(
            RouteProbeRequest::decode(&invalid_mode),
            Err(ProtocolError::InvalidMode(_))
        ));

        let mut unknown_relationship = frame;
        unknown_relationship[HEADER_LEN + 18..HEADER_LEN + 20]
            .copy_from_slice(&99_u16.to_le_bytes());
        assert_eq!(
            RouteProbeRequest::decode(&unknown_relationship),
            Err(ProtocolError::UnknownRelationship(99))
        );

        let mut invalid_absent_sink = request(8, false).encode();
        invalid_absent_sink[HEADER_LEN + 72..HEADER_LEN + 76]
            .copy_from_slice(&226_u32.to_le_bytes());
        assert_eq!(
            RouteProbeRequest::decode(&invalid_absent_sink),
            Err(ProtocolError::NonCanonicalAbsentSink)
        );

        let mut zero_connector = frame;
        zero_connector[HEADER_LEN + 112..HEADER_LEN + 116].copy_from_slice(&0_u32.to_le_bytes());
        assert_eq!(
            RouteProbeRequest::decode(&zero_connector),
            Err(ProtocolError::ZeroKmsHandle("connector"))
        );

        let mut zero_timeout = frame;
        zero_timeout[HEADER_LEN + 128..HEADER_LEN + 136].copy_from_slice(&0_u64.to_le_bytes());
        assert_eq!(
            RouteProbeRequest::decode(&zero_timeout),
            Err(ProtocolError::ZeroFenceTimeout)
        );
    }

    #[test]
    fn response_codec_rejects_noncanonical_outcomes() {
        let shared = RouteProbeResponse {
            token: CrtcConfigToken(7),
            outcome: RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Shared(
                ScanoutAllocationPlan::ExplicitLinear,
            )),
            elapsed_ns: 1,
        };
        let mut zero_token = shared.encode();
        zero_token[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&0_u64.to_le_bytes());
        assert_eq!(
            RouteProbeResponse::decode(&zero_token),
            Err(ProtocolError::ZeroToken)
        );

        let mut compatible_with_error = shared.encode();
        compatible_with_error[HEADER_LEN + 28..HEADER_LEN + 32]
            .copy_from_slice(&1_u32.to_le_bytes());
        assert_eq!(
            RouteProbeResponse::decode(&compatible_with_error),
            Err(ProtocolError::CompatibleCarriesFailure)
        );

        let mut valueless_plan_with_value = shared.encode();
        valueless_plan_with_value[HEADER_LEN + 32..HEADER_LEN + 40]
            .copy_from_slice(&1_u64.to_le_bytes());
        assert_eq!(
            RouteProbeResponse::decode(&valueless_plan_with_value),
            Err(ProtocolError::NonCanonicalPlanValue { kind: 4, value: 1 })
        );

        let rejected = RouteProbeResponse {
            token: CrtcConfigToken(7),
            outcome: RouteProbeOutcome::Rejected(ProbeFailure {
                error_code: 5,
                detail_code: 6,
            }),
            elapsed_ns: 1,
        };
        let mut failure_with_plan = rejected.encode();
        failure_with_plan[HEADER_LEN + 10..HEADER_LEN + 12].copy_from_slice(&1_u16.to_le_bytes());
        assert_eq!(
            RouteProbeResponse::decode(&failure_with_plan),
            Err(ProtocolError::FailureCarriesPlan {
                status: ProbeStatus::Rejected
            })
        );

        let request = request(7, true);
        let mut incomplete_copied = copied_response(&request).encode();
        incomplete_copied[HEADER_LEN + 16..HEADER_LEN + 18].copy_from_slice(&0_u16.to_le_bytes());
        assert_eq!(
            RouteProbeResponse::decode(&incomplete_copied),
            Err(ProtocolError::IncompleteCopiedPlan)
        );
    }

    #[test]
    fn response_validation_rejects_wrong_token_or_copied_sink() {
        let request = request(7, true);
        let mut response = copied_response(&request);
        response.token = CrtcConfigToken(8);
        assert_eq!(
            validate_response_for_request(&request, &response),
            Err(ProtocolError::MismatchedToken {
                actual: 8,
                expected: 7
            })
        );

        response.token = request.token;
        response.outcome = RouteProbeOutcome::Compatible(QualifiedScanoutPlan::Copied {
            sink_id: drm_render(226, 130),
            plan: CopiedScanoutPlan {
                source: CopiedSourcePlan::DrmModifier(0),
                destination: ScanoutAllocationPlan::LegacyLinear,
            },
        });
        assert!(matches!(
            validate_response_for_request(&request, &response),
            Err(ProtocolError::MismatchedCopiedSink { .. })
        ));
    }

    #[test]
    fn injected_helper_handler_round_trips_over_control_socket() {
        let (mut parent, child) = UnixStream::pair().expect("socketpair");
        let inherited: OwnedFd = File::open("/dev/null").expect("open /dev/null").into();
        let device = crate::drm::Device::from_inherited_kms_fd(inherited, "/dev/null");
        let worker = thread::spawn(move || {
            serve_one(child, device, |device, request| {
                assert!(device.as_fd().as_raw_fd() >= 0);
                copied_response(&request)
            })
        });

        let request = request(7, true);
        parent.write_all(&request.encode()).expect("send request");
        let mut frame = [0_u8; RESPONSE_FRAME_LEN];
        parent.read_exact(&mut frame).expect("read response");
        assert_eq!(
            RouteProbeResponse::decode(&frame),
            Ok(copied_response(&request))
        );
        worker.join().expect("helper thread").expect("serve helper");
    }

    #[test]
    fn supervisor_accepts_response_without_waiting_for_child_exit() {
        let (parent, mut helper) = UnixStream::pair().expect("socketpair");
        let request = request(11, true);
        let worker = thread::spawn(move || {
            let mut frame = [0_u8; REQUEST_FRAME_LEN];
            helper.read_exact(&mut frame).expect("read request");
            let decoded = RouteProbeRequest::decode(&frame).expect("decode request");
            helper
                .write_all(&copied_response(&decoded).encode())
                .expect("write response");
        });
        let child = Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleeping child");

        let started = Instant::now();
        let observed = supervise_exchange(child, parent, request, Duration::from_secs(1))
            .expect("supervisor response");
        assert_eq!(observed, copied_response(&request));
        assert!(started.elapsed() < Duration::from_secs(1));
        worker.join().expect("response writer");
    }

    #[test]
    fn supervisor_reaps_child_when_post_spawn_deadline_setup_fails() {
        assert!(
            Instant::now().checked_add(Duration::MAX).is_none(),
            "Duration::MAX must overflow this platform's Instant"
        );
        let (parent, _helper) = UnixStream::pair().expect("socketpair");
        let child = Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleeping child");
        let child_pid = libc::pid_t::try_from(child.id()).expect("child pid fits pid_t");

        let error = supervise_exchange(child, parent, request(12, true), Duration::MAX)
            .expect_err("deadline overflow must fail setup");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let reap_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            // SAFETY: signal 0 does not affect the process; it only checks
            // whether the child PID still names a live or zombie process.
            let present = unsafe { libc::kill(child_pid, 0) } == 0;
            if !present && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            assert!(
                Instant::now() < reap_deadline,
                "supervisor cleanup did not reap setup-failed child {child_pid}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn supervisor_times_out_on_partial_response() {
        let (parent, mut helper) = UnixStream::pair().expect("socketpair");
        let (request_seen_tx, request_seen_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut request_frame = [0_u8; REQUEST_FRAME_LEN];
            helper.read_exact(&mut request_frame).expect("read request");
            request_seen_tx.send(()).expect("report request");
            let request = RouteProbeRequest::decode(&request_frame).expect("decode request");
            let response = copied_response(&request).encode();
            helper
                .write_all(&response[..HEADER_LEN])
                .expect("write partial response");
            thread::sleep(Duration::from_millis(100));
        });
        let child = Command::new("/bin/sleep")
            .arg("10")
            .spawn()
            .expect("spawn sleeping child");

        let started = Instant::now();
        let error = supervise_exchange(child, parent, request(13, true), Duration::from_millis(30))
            .expect_err("partial response must hit watchdog");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        request_seen_rx.recv().expect("request reached helper");
        worker.join().expect("partial response writer");
    }
}
