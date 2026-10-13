//! Diagnostic event log for every `vkQueueSubmit2` call site on
//! the v2 path. Written for Stage 5 Task 3 (paint-submit
//! aggregation) — characterise the live submit traffic by kind +
//! target + render key before designing the aggregation boundary,
//! per the timeline-semaphore lesson (don't design without
//! per-op data).
//!
//! Off by default. Enable by setting
//! `YSERVER_SUBMIT_TRACE=/path/to/trace.tsv` before launching
//! yserver — the file is truncated at startup, a header line is
//! written, and every subsequent submit appends one TSV row.
//!
//! Format: one header line + one row per submit, all tabs as
//! separator, 14 columns:
//!
//! ```text
//! frame_id  ns_mono  kind  target_kind  target_id  batch_size  \
//!     op  src_class  mask_class  pipeline_id  \
//!     readback  alias  zero_draws  upload
//! ```
//!
//! Non-render kinds emit `-` for `op` / `src_class` / `mask_class` /
//! `pipeline_id`. Symbolic enum names throughout (no raw numbers
//! or packed bitfields) so `awk -F'\t' '$3=="render_composite"
//! && $7=="over"'` works without a decoder.
//!
//! Single-threaded core: `Telemetry` is reached via `&mut self`
//! everywhere, so `SubmitTrace` is held as a plain field — no
//! interior mutability needed.

use std::{
    fmt,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    time::Instant,
};

/// One row in the trace file. Built at each `record_submit_event`
/// call site; the trace owns the cost of formatting + writing.
#[derive(Debug, Clone, Copy)]
pub struct SubmitEvent {
    pub frame_id: u64,
    pub kind: SubmitKind,
    pub target_kind: TargetKind,
    pub target_id: u64,
    pub batch_size: u32,
    /// Picture / GC operator. `Op::None` writes `-`.
    pub op: Op,
    pub src_class: SrcClass,
    pub mask_class: SrcClass,
    /// Index into the engine's render-pipeline cache for RENDER
    /// kinds. `None` writes `-`.
    pub pipeline_id: Option<u32>,
    pub flags: Flags,
}

/// Single submit kind per `vkQueueSubmit2` call site, grouped by
/// engine method (not X11 source path — the engine method is what
/// determines hot-path cost; X11 source is recoverable from
/// `target_id` + render fields).
///
/// Names chosen for grep ergonomics: no name is a strict prefix of
/// another, so `grep -c render_composite` matches only one kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitKind {
    FillOne,
    FillBatch,
    LogicFill,
    CopyArea,
    PutImage,
    RenderComposite,
    RenderFill,
    RenderTraps,
    RenderTris,
    CompositeGlyphs,
    ImageText,
    GlyphUpload,
    GetImage,
    CopyPlaneRb,
    SceneCompose,
}

impl fmt::Display for SubmitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::FillOne => "fill_one",
            Self::FillBatch => "fill_batch",
            Self::LogicFill => "logic_fill",
            Self::CopyArea => "copy_area",
            Self::PutImage => "put_image",
            Self::RenderComposite => "render_composite",
            Self::RenderFill => "render_fill",
            Self::RenderTraps => "render_traps",
            Self::RenderTris => "render_tris",
            Self::CompositeGlyphs => "composite_glyphs",
            Self::ImageText => "image_text",
            Self::GlyphUpload => "glyph_upload",
            Self::GetImage => "get_image",
            Self::CopyPlaneRb => "copy_plane_rb",
            Self::SceneCompose => "scene_compose",
        };
        f.write_str(s)
    }
}

/// What kind of drawable / sink the submit's `target_id` refers
/// to. `Output` is used for `scene_compose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Window,
    Pixmap,
    Root,
    Cow,
    Cursor,
    Backing,
    Output,
    Unknown,
}

impl fmt::Display for TargetKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Window => "window",
            Self::Pixmap => "pixmap",
            Self::Root => "root",
            Self::Cow => "cow",
            Self::Cursor => "cursor",
            Self::Backing => "backing",
            Self::Output => "output",
            Self::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// `PictOp` / `GcFunction` symbolic name. `None` writes `-` and means
/// "this kind has no operator" (e.g. `put_image`, `copy_area`).
///
/// Covers the RENDER operators yserver dispatches plus a handful
/// of GC `GXfunction` values for `LogicFill`. Anything we haven't
/// named explicitly falls through to `Other(u8)` which prints as
/// `op_0xNN` — still grep-able, just opaque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    None,
    // RENDER PictOps
    Clear,
    Src,
    Dst,
    Over,
    OverReverse,
    In,
    InReverse,
    Out,
    OutReverse,
    Atop,
    AtopReverse,
    Xor,
    Add,
    Saturate,
    DisjointOver,
    ConjointOver,
    // GC GXfunction (LogicFill kind only)
    GxCopy,
    GxClear,
    GxAnd,
    GxOr,
    GxXor,
    GxInvert,
    GxSet,
    Other(u8),
}

impl Op {
    /// Map an X11 `GXfunction` protocol byte (per X11 spec
    /// §"Graphics Context Components") to the symbolic Gx*
    /// variant. Used by `LogicFill` events. Values outside the
    /// common subset fall through to `Other(byte)`.
    #[must_use]
    pub fn from_gx_byte(b: u8) -> Self {
        match b {
            0 => Self::GxClear,
            1 => Self::GxAnd,
            3 => Self::GxCopy,
            6 => Self::GxXor,
            7 => Self::GxOr,
            10 => Self::GxInvert,
            15 => Self::GxSet,
            other => Self::Other(other),
        }
    }

    /// Map a RENDER `PictOp` wire byte to the symbolic variant.
    /// Wire values 0..=13 cover Clear..Saturate; 0x10..=0x12 are
    /// the standard Disjoint Over family; conjoint family starts
    /// at 0x20. Anything else falls through to `Other(byte)`
    /// which prints as `op_0xNN` — grep-able, just opaque.
    #[must_use]
    pub fn from_pict_op_byte(b: u8) -> Self {
        match b {
            0 => Self::Clear,
            1 => Self::Src,
            2 => Self::Dst,
            3 => Self::Over,
            4 => Self::OverReverse,
            5 => Self::In,
            6 => Self::InReverse,
            7 => Self::Out,
            8 => Self::OutReverse,
            9 => Self::Atop,
            10 => Self::AtopReverse,
            11 => Self::Xor,
            12 => Self::Add,
            13 => Self::Saturate,
            // 0x10 family — disjoint
            0x18 => Self::DisjointOver,
            // 0x20 family — conjoint
            0x28 => Self::ConjointOver,
            other => Self::Other(other),
        }
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("-"),
            Self::Clear => f.write_str("clear"),
            Self::Src => f.write_str("src"),
            Self::Dst => f.write_str("dst"),
            Self::Over => f.write_str("over"),
            Self::OverReverse => f.write_str("over_reverse"),
            Self::In => f.write_str("in"),
            Self::InReverse => f.write_str("in_reverse"),
            Self::Out => f.write_str("out"),
            Self::OutReverse => f.write_str("out_reverse"),
            Self::Atop => f.write_str("atop"),
            Self::AtopReverse => f.write_str("atop_reverse"),
            Self::Xor => f.write_str("xor"),
            Self::Add => f.write_str("add"),
            Self::Saturate => f.write_str("saturate"),
            Self::DisjointOver => f.write_str("disjoint_over"),
            Self::ConjointOver => f.write_str("conjoint_over"),
            Self::GxCopy => f.write_str("gx_copy"),
            Self::GxClear => f.write_str("gx_clear"),
            Self::GxAnd => f.write_str("gx_and"),
            Self::GxOr => f.write_str("gx_or"),
            Self::GxXor => f.write_str("gx_xor"),
            Self::GxInvert => f.write_str("gx_invert"),
            Self::GxSet => f.write_str("gx_set"),
            Self::Other(v) => write!(f, "op_0x{v:02x}"),
        }
    }
}

/// Classifies a Picture record source (or mask) for the
/// aggregation key. `None` for kinds with no source/mask
/// (writes `-`); `NoMask` distinguishes "mask is absent" from
/// "mask exists but unknown class".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrcClass {
    None,
    NoMask,
    Direct,
    Solid,
    GradientLinear,
    GradientRadial,
}

impl fmt::Display for SrcClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::None => "-",
            Self::NoMask => "no_mask",
            Self::Direct => "direct",
            Self::Solid => "solid",
            Self::GradientLinear => "gradient_linear",
            Self::GradientRadial => "gradient_radial",
        };
        f.write_str(s)
    }
}

/// Per-event boolean flags. Each becomes a `0`/`1` column in the
/// TSV so `awk '$11==1'` filters trivially.
#[allow(
    clippy::struct_excessive_bools,
    reason = "Each bool is an independent per-event TSV column; packing into a bitfield would defeat the grep-friendly schema."
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// Dst readback fired (Disjoint/Conjoint composite path).
    pub readback: bool,
    /// Src self-alias scratch was used.
    pub alias: bool,
    /// Submitted a CB that recorded zero draws (worth measuring —
    /// would mean a "wasted" submit).
    pub zero_draws: bool,
    /// Submit also did a staging upload (`image_text` /
    /// `composite_glyphs` combined paint+upload paths).
    pub upload: bool,
}

impl Flags {
    /// Convenience all-false constant for sites that have no
    /// applicable flags (most paint paths).
    pub const NONE: Self = Self {
        readback: false,
        alias: false,
        zero_draws: false,
        upload: false,
    };
}

/// Header line written verbatim once on file open. Kept as a
/// constant so the unit test can assert byte-equality.
pub const HEADER: &str = "frame_id\tns_mono\tkind\ttarget_kind\ttarget_id\tbatch_size\
\top\tsrc_class\tmask_class\tpipeline_id\treadback\talias\tzero_draws\tupload\n";

/// Owns the trace file. Constructed once at `Telemetry::new`
/// from the env var; `None` if the env var is absent or the
/// file can't be opened. Drop flushes via `BufWriter`'s own
/// drop.
pub struct SubmitTrace {
    writer: BufWriter<File>,
    start: Instant,
    /// Set after the first write error so we don't spam the log
    /// every submit if the disk fills or the fd dies. Subsequent
    /// `record` calls become no-ops.
    write_failed: bool,
}

impl SubmitTrace {
    /// Construct from `YSERVER_SUBMIT_TRACE`. Returns `None`
    /// when the env var is absent or empty (the common case;
    /// zero hot-path cost for non-tracing runs).
    ///
    /// On open failure (bad path, permissions), logs one
    /// `warn!` and returns `None` — yserver continues normally
    /// without tracing.
    #[must_use]
    pub(crate) fn from_env() -> Option<Self> {
        let raw = std::env::var_os("YSERVER_SUBMIT_TRACE")?;
        let path_str = raw.to_str()?;
        if path_str.is_empty() {
            return None;
        }
        let path = PathBuf::from(path_str);
        match File::create(&path) {
            Ok(file) => {
                let mut writer = BufWriter::new(file);
                if let Err(err) = writer.write_all(HEADER.as_bytes()) {
                    log::warn!(
                        "submit_trace: failed to write header to {}: {err}; \
                         tracing disabled",
                        path.display(),
                    );
                    return None;
                }
                log::info!("submit_trace: writing to {}", path.display());
                Some(Self {
                    writer,
                    start: Instant::now(),
                    write_failed: false,
                })
            }
            Err(err) => {
                log::warn!(
                    "submit_trace: failed to open {}: {err}; tracing disabled",
                    path.display(),
                );
                None
            }
        }
    }

    /// Flush the underlying `BufWriter`. Called periodically (1Hz
    /// via `Telemetry::maybe_emit`) and explicitly during shutdown
    /// (`KmsBackend::disable_output`) so a hung drop chain or
    /// hard kill doesn't lose the buffered tail. A failed flush
    /// disables further writes for the rest of the process,
    /// mirroring `record`'s self-quiescing behaviour.
    pub(crate) fn flush(&mut self) {
        if self.write_failed {
            return;
        }
        if let Err(err) = self.writer.flush() {
            log::warn!("submit_trace: flush failed: {err}; suppressing further trace output");
            self.write_failed = true;
        }
    }

    /// Append one event. After the first write error, becomes a
    /// silent no-op for the remainder of the process — we don't
    /// want to spam the log every submit if the disk fills up.
    pub(crate) fn record(&mut self, event: &SubmitEvent) {
        if self.write_failed {
            return;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "u128 ns → u64 is fine for any practical session length"
        )]
        let ns_mono = self.start.elapsed().as_nanos() as u64;
        let pipe = match event.pipeline_id {
            Some(id) => i64::from(id),
            None => -1,
        };
        let res = write!(
            self.writer,
            "{frame}\t{ns}\t{kind}\t{tk}\t{tid}\t{bs}\t{op}\t{src}\t{mask}\t",
            frame = event.frame_id,
            ns = ns_mono,
            kind = event.kind,
            tk = event.target_kind,
            tid = event.target_id,
            bs = event.batch_size,
            op = event.op,
            src = event.src_class,
            mask = event.mask_class,
        )
        .and_then(|()| {
            if pipe < 0 {
                self.writer.write_all(b"-")
            } else {
                write!(self.writer, "{pipe}")
            }
        })
        .and_then(|()| {
            writeln!(
                self.writer,
                "\t{rb}\t{al}\t{zd}\t{up}",
                rb = u8::from(event.flags.readback),
                al = u8::from(event.flags.alias),
                zd = u8::from(event.flags.zero_draws),
                up = u8::from(event.flags.upload),
            )
        });
        if let Err(err) = res {
            log::warn!("submit_trace: write failed: {err}; suppressing further trace output");
            self.write_failed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempPath(PathBuf);
    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn unique_temp_path() -> TempPath {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = Instant::now().elapsed().as_nanos();
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("yserver-submit-trace-{pid}-{nanos}-{n}.tsv"));
        TempPath(path)
    }

    fn make_trace_at(path: &Path) -> SubmitTrace {
        let file = File::create(path).expect("open");
        let mut writer = BufWriter::new(file);
        writer.write_all(HEADER.as_bytes()).expect("hdr");
        SubmitTrace {
            writer,
            start: Instant::now(),
            write_failed: false,
        }
    }

    fn read_back(path: &Path) -> String {
        let mut s = String::new();
        let mut f = File::open(path).expect("reopen");
        f.read_to_string(&mut s).expect("read");
        s
    }

    #[test]
    fn from_env_returns_none_when_unset() {
        // Use a safe assumption: scoped to this thread via SAFETY
        // of std::env in a test that doesn't fork. We
        // unconditionally clear, run, and don't restore — other
        // tests don't depend on this var.
        // SAFETY: tests run single-threaded for this module and we
        // don't rely on the env var elsewhere in the harness.
        unsafe {
            std::env::remove_var("YSERVER_SUBMIT_TRACE");
        }
        assert!(SubmitTrace::from_env().is_none());
    }

    #[test]
    fn from_env_returns_none_when_empty() {
        // SAFETY: see from_env_returns_none_when_unset.
        unsafe {
            std::env::set_var("YSERVER_SUBMIT_TRACE", "");
        }
        let r = SubmitTrace::from_env();
        // SAFETY: see from_env_returns_none_when_unset.
        unsafe {
            std::env::remove_var("YSERVER_SUBMIT_TRACE");
        }
        assert!(r.is_none());
    }

    #[test]
    fn header_format_is_stable() {
        // Lock the column layout. Downstream awk scripts depend
        // on these exact column positions.
        assert_eq!(
            HEADER,
            "frame_id\tns_mono\tkind\ttarget_kind\ttarget_id\tbatch_size\
\top\tsrc_class\tmask_class\tpipeline_id\treadback\talias\tzero_draws\tupload\n"
        );
    }

    #[test]
    fn record_writes_one_tsv_row_with_all_columns() {
        let path = unique_temp_path();
        let mut trace = make_trace_at(&path.0);
        let event = SubmitEvent {
            frame_id: 7,
            kind: SubmitKind::RenderComposite,
            target_kind: TargetKind::Window,
            target_id: 42,
            batch_size: 3,
            op: Op::Over,
            src_class: SrcClass::Solid,
            mask_class: SrcClass::NoMask,
            pipeline_id: Some(17),
            flags: Flags {
                readback: false,
                alias: true,
                zero_draws: false,
                upload: false,
            },
        };
        trace.record(&event);
        trace.writer.flush().expect("flush");
        drop(trace);
        let body = read_back(&path.0);
        // First line is the header (14 columns, tab-separated).
        let mut lines = body.lines();
        let hdr = lines.next().expect("header");
        assert_eq!(hdr.split('\t').count(), 14);
        // Second line is the event.
        let row = lines.next().expect("row");
        let fields: Vec<&str> = row.split('\t').collect();
        assert_eq!(fields.len(), 14);
        assert_eq!(fields[0], "7");
        // fields[1] is ns_mono — non-deterministic, but must parse.
        let _ns: u64 = fields[1].parse().expect("ns_mono parses");
        assert_eq!(fields[2], "render_composite");
        assert_eq!(fields[3], "window");
        assert_eq!(fields[4], "42");
        assert_eq!(fields[5], "3");
        assert_eq!(fields[6], "over");
        assert_eq!(fields[7], "solid");
        assert_eq!(fields[8], "no_mask");
        assert_eq!(fields[9], "17");
        assert_eq!(fields[10], "0");
        assert_eq!(fields[11], "1");
        assert_eq!(fields[12], "0");
        assert_eq!(fields[13], "0");
        // No trailing third line.
        assert!(lines.next().is_none());
    }

    #[test]
    fn record_emits_dash_for_pipeline_none_and_non_render_kinds() {
        let path = unique_temp_path();
        let mut trace = make_trace_at(&path.0);
        let event = SubmitEvent {
            frame_id: 0,
            kind: SubmitKind::FillOne,
            target_kind: TargetKind::Pixmap,
            target_id: 5,
            batch_size: 1,
            op: Op::None,
            src_class: SrcClass::None,
            mask_class: SrcClass::None,
            pipeline_id: None,
            flags: Flags::NONE,
        };
        trace.record(&event);
        trace.writer.flush().expect("flush");
        drop(trace);
        let body = read_back(&path.0);
        let row = body.lines().nth(1).expect("row");
        let fields: Vec<&str> = row.split('\t').collect();
        assert_eq!(fields[2], "fill_one");
        assert_eq!(fields[6], "-");
        assert_eq!(fields[7], "-");
        assert_eq!(fields[8], "-");
        assert_eq!(fields[9], "-");
    }

    #[test]
    fn flush_commits_buffered_writes_to_disk() {
        // The load-bearing regression test: BufWriter's default
        // 8KB buffer means small captures (e.g. a 2-second drag
        // truncated by zap) leave the file at 0 bytes until either
        // Drop or an explicit flush. The 2026-05-22 yoga zap-hang
        // scenario lost the entire trace because the drop chain
        // hung before BufWriter::Drop could run. flush() called at
        // shutdown (or 1Hz via maybe_emit) is what closes that
        // window.
        let path = unique_temp_path();
        let mut trace = make_trace_at(&path.0);
        let event = SubmitEvent {
            frame_id: 1,
            kind: SubmitKind::FillOne,
            target_kind: TargetKind::Pixmap,
            target_id: 9,
            batch_size: 1,
            op: Op::None,
            src_class: SrcClass::None,
            mask_class: SrcClass::None,
            pipeline_id: None,
            flags: Flags::NONE,
        };
        trace.record(&event);
        // Pre-flush: only the test fixture's header is on disk
        // (the fixture flushes it as part of `make_trace_at`). The
        // recorded event lives in the BufWriter and is invisible
        // on disk until flush() runs. Assert the row is absent.
        let before = read_back(&path.0);
        assert!(
            !before.contains("fill_one"),
            "row leaked to disk before flush: {before:?}",
        );

        trace.flush();
        // Post-flush: read_back without dropping `trace`. The row
        // must now be on disk; this is the property a shutdown-
        // path flush relies on.
        let after = read_back(&path.0);
        let row = after.lines().nth(1).expect("event row after flush");
        let fields: Vec<&str> = row.split('\t').collect();
        assert_eq!(fields.len(), 14);
        assert_eq!(fields[2], "fill_one");
        assert_eq!(fields[4], "9");
    }

    #[test]
    fn flush_after_write_failure_is_silent_noop() {
        // Post-write-failure self-quiescing: once `write_failed`
        // is latched, flush() must not retry or log. Mirrors the
        // record() behaviour so a single error in the run doesn't
        // produce one warn line per second from maybe_emit's
        // periodic flush.
        let path = unique_temp_path();
        let mut trace = make_trace_at(&path.0);
        trace.write_failed = true;
        trace.flush();
        // Sanity: the writer wasn't touched. Header from fixture
        // is still the only content (file may be 0 bytes since the
        // fixture buffers the header too, but no panic / no log).
        let _ = read_back(&path.0);
    }

    #[test]
    fn kind_names_have_no_prefix_collisions() {
        // Property: no SubmitKind variant's printed name is a
        // strict prefix of another's. Guarantees grep -c <name>
        // doesn't double-count.
        let all = [
            SubmitKind::FillOne,
            SubmitKind::FillBatch,
            SubmitKind::LogicFill,
            SubmitKind::CopyArea,
            SubmitKind::PutImage,
            SubmitKind::RenderComposite,
            SubmitKind::RenderFill,
            SubmitKind::RenderTraps,
            SubmitKind::RenderTris,
            SubmitKind::CompositeGlyphs,
            SubmitKind::ImageText,
            SubmitKind::GlyphUpload,
            SubmitKind::GetImage,
            SubmitKind::CopyPlaneRb,
            SubmitKind::SceneCompose,
        ];
        let names: Vec<String> = all.iter().map(ToString::to_string).collect();
        for (i, a) in names.iter().enumerate() {
            for (j, b) in names.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    !b.starts_with(a) || a == b,
                    "kind name `{a}` is a prefix of `{b}` — would break grep"
                );
            }
        }
    }
}
