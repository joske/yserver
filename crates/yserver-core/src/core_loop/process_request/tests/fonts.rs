//! Core font lifetime as in Xorg: `CloseFont` frees the id
//! (`dix/dispatch.c:1319`), the font lives while a GC holds it
//! (`pFont->refcnt`, `dix/dixfonts.c:462`; `dix/gc.c:290`, `:697`, `:771`).

use super::*;
use crate::{
    backend::recording::RecordedCall,
    resources::{DEFAULT_FONT, ROOT_WINDOW},
};

const F: u32 = 0x0010_0010;
const G: u32 = 0x0010_0011;
const GC1: u32 = 0x0010_0001;
const GC2: u32 = 0x0010_0002;

struct Fx {
    state: ServerState,
    backend: RecordingBackend,
    peers: Vec<UnixStream>,
    seq: u16,
}

impl Fx {
    fn new() -> Self {
        let mut state = ServerState::new();
        let mut backend = RecordingBackend::new();
        crate::core_loop::process_request::install_default_font(&mut state, &mut backend)
            .expect("default font");
        let peers = vec![install_client(&mut state, 1), install_client(&mut state, 2)];
        Self {
            state,
            backend,
            peers,
            seq: 0,
        }
    }

    fn req_as(&mut self, client: u32, opcode: u8, words: &[u32]) -> Vec<u8> {
        let body: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.raw_as(client, opcode, &body)
    }

    fn raw_as(&mut self, client: u32, opcode: u8, body: &[u8]) -> Vec<u8> {
        self.seq += 1;
        process_request(
            &mut self.state,
            &mut self.backend,
            ClientId(client),
            SequenceNumber(self.seq),
            RequestHeader {
                opcode,
                data: 0,
                length_units: u32::try_from(body.len() / 4 + 1).unwrap(),
            },
            body,
            None,
        )
        .expect("process_request");
        let i = usize::try_from(client - 1).unwrap();
        read_all_available(&mut self.peers[i])
    }

    fn req(&mut self, opcode: u8, words: &[u32]) -> Vec<u8> {
        self.req_as(1, opcode, words)
    }

    fn open_font_as(&mut self, client: u32, fid: u32, name: &str) {
        let mut body = fid.to_le_bytes().to_vec();
        body.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(name.as_bytes());
        body.resize(body.len().next_multiple_of(4), 0);
        assert!(self.raw_as(client, 45, &body).is_empty(), "OpenFont failed");
    }

    fn open_font(&mut self, fid: u32, name: &str) {
        self.open_font_as(1, fid, name);
    }

    /// CreateGC on the root with GCFont = `font`, or no values at all.
    fn create_gc_as(&mut self, client: u32, gc: u32, font: Option<u32>) -> Vec<u8> {
        match font {
            Some(font) => self.req_as(client, 55, &[gc, ROOT_WINDOW.0, 0x4000, font]),
            None => self.req_as(client, 55, &[gc, ROOT_WINDOW.0, 0]),
        }
    }

    fn host(&self, fid: u32) -> u32 {
        self.state
            .resources
            .font(ResourceId(fid))
            .expect("open font")
            .host_xid
            .as_raw()
    }

    fn gc_host(&self, gc: u32) -> Option<u32> {
        self.state
            .resources
            .resolve_draw_state(ResourceId(gc))
            .and_then(|s| s.font)
            .map(|f| f.as_raw())
    }

    fn closes(&self, host: u32) -> usize {
        self.backend
            .calls()
            .iter()
            .filter(|c| matches!(c, RecordedCall::CloseFont(x) if *x == host))
            .count()
    }

    fn query_font(&mut self, fontable: u32) -> Vec<u8> {
        self.req(47, &[fontable])
    }
}

fn assert_bad_font(bytes: &[u8], id: u32, opcode: u8) {
    assert!(bytes.len() >= 32, "expected an error, got {bytes:02x?}");
    assert_eq!((bytes[0], bytes[1]), (0, x11::error::BAD_FONT), "BadFont");
    assert_eq!(&bytes[4..8], &id.to_le_bytes(), "bad value");
    assert_eq!(bytes[10], opcode, "major opcode");
}

/// A QueryFont reply with its sequence number masked.
fn font_reply(mut bytes: Vec<u8>) -> Vec<u8> {
    assert_eq!(
        bytes.first(),
        Some(&1),
        "a reply, not an error: {bytes:02x?}"
    );
    bytes[2..4].fill(0);
    bytes
}

/// `ProcCloseFont` looks the id up as RT_FONT: an unknown id and a GContext
/// are both BadFont with the id as value.
#[test]
fn close_font_of_a_non_font_id_is_bad_font() {
    let mut fx = Fx::new();
    assert_bad_font(&fx.req(46, &[0xdead_beef]), 0xdead_beef, 46);
    assert!(fx.create_gc_as(1, GC1, None).is_empty());
    assert_bad_font(&fx.req(46, &[GC1]), GC1, 46);
}

/// The id is freed: closing it again is BadFont, and it can name a new font.
#[test]
fn close_font_frees_the_id() {
    let mut fx = Fx::new();
    fx.open_font(F, "fixed");
    assert!(fx.req(46, &[F]).is_empty());
    assert_bad_font(&fx.req(46, &[F]), F, 46);
    assert_bad_font(&fx.query_font(F), F, 47);
    fx.open_font(F, "cursor");
}

/// A GC keeps its font after `CloseFont`: it still draws with it, QueryFont
/// on the GContext still answers, and the font goes with the last GC.
#[test]
fn a_gc_keeps_its_font_after_close_font() {
    let mut fx = Fx::new();
    let baseline = fx.state.resources.live_font_count();
    fx.open_font(F, "cursor");
    let host = fx.host(F);
    let before = font_reply(fx.query_font(F));
    assert!(fx.create_gc_as(1, GC1, Some(F)).is_empty());
    assert!(fx.req(46, &[F]).is_empty());
    assert_eq!(fx.closes(host), 0, "closed while a GC holds it");
    assert_eq!(fx.gc_host(GC1), Some(host));
    assert_eq!(font_reply(fx.query_font(GC1)), before);
    assert!(fx.req(60, &[GC1]).is_empty());
    assert_eq!(fx.closes(host), 1, "the last GC releases the font");
    assert_eq!(fx.state.resources.live_font_count(), baseline);
}

/// After `CloseFont` + `OpenFont` of another font under the same id, the old
/// GC still has the old font and a new GC gets the new one.
#[test]
fn a_reused_font_id_does_not_retarget_an_old_gc() {
    let mut fx = Fx::new();
    fx.open_font(F, "cursor");
    let old_host = fx.host(F);
    let old = font_reply(fx.query_font(F));
    assert!(fx.create_gc_as(1, GC1, Some(F)).is_empty());
    assert!(fx.req(46, &[F]).is_empty());
    fx.open_font(F, "fixed");
    let new_host = fx.host(F);
    let new = font_reply(fx.query_font(F));
    assert_ne!(old, new, "the two fonts must differ to prove anything");
    assert!(fx.create_gc_as(1, GC2, Some(F)).is_empty());
    assert_eq!(fx.gc_host(GC1), Some(old_host));
    assert_eq!(fx.gc_host(GC2), Some(new_host));
    assert_eq!(font_reply(fx.query_font(GC1)), old);
    assert_eq!(font_reply(fx.query_font(GC2)), new);
}

/// CopyGC takes a reference, ChangeGC and a PolyText font item drop the
/// old one; the font goes when the last of them lets go.
#[test]
fn copy_change_and_poly_text_move_font_references() {
    let mut fx = Fx::new();
    fx.open_font(F, "cursor");
    fx.open_font(G, "fixed");
    let host = fx.host(F);
    assert!(fx.create_gc_as(1, GC1, Some(F)).is_empty());
    assert!(fx.create_gc_as(1, GC2, None).is_empty());
    assert!(fx.req(57, &[GC1, GC2, 0x4000]).is_empty(), "CopyGC");
    assert!(fx.req(46, &[F]).is_empty());
    assert!(fx.req(56, &[GC1, 0x4000, G]).is_empty(), "ChangeGC");
    assert_eq!(fx.closes(host), 0, "GC2 still holds it");
    assert_eq!(fx.gc_host(GC2), Some(host));
    // PolyText8 on GC2 with one font item, G.
    let mut body: Vec<u8> = [ROOT_WINDOW.0, GC2]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    body.extend_from_slice(&[0, 0, 10, 0, 255]);
    body.extend_from_slice(&G.to_be_bytes());
    body.resize(body.len().next_multiple_of(4), 0);
    assert!(fx.raw_as(1, 74, &body).is_empty(), "PolyText8");
    assert_eq!(fx.gc_host(GC2), Some(fx.host(G)));
    assert_eq!(fx.closes(host), 1);
}

/// `ChangeGCXIDs` resolves GCFont before changing anything: BadFont, and
/// CreateGC makes no GC.
#[test]
fn create_and_change_gc_with_an_unknown_font_are_bad_font() {
    let mut fx = Fx::new();
    assert_bad_font(&fx.create_gc_as(1, GC1, Some(F)), F, 55);
    assert!(fx.state.resources.gc(ResourceId(GC1)).is_none());
    assert!(fx.create_gc_as(1, GC1, None).is_empty());
    // GCForeground | GCFont.
    assert_bad_font(&fx.req(56, &[GC1, 0x4004, 0x00ab_cdef, F]), F, 56);
    assert_eq!(fx.state.resources.gc_foreground(ResourceId(GC1)), 0);
}

/// The default font's id can be closed like any other (Xorg's is a plain
/// server-client resource); new GCs still get the font.
#[test]
fn close_font_of_the_default_font_frees_only_its_id() {
    let mut fx = Fx::new();
    let host = fx.host(DEFAULT_FONT.0);
    assert!(fx.create_gc_as(1, GC1, None).is_empty());
    assert!(fx.req(46, &[DEFAULT_FONT.0]).is_empty());
    assert_bad_font(&fx.query_font(DEFAULT_FONT.0), DEFAULT_FONT.0, 47);
    assert_bad_font(&fx.req(46, &[DEFAULT_FONT.0]), DEFAULT_FONT.0, 46);
    assert!(fx.create_gc_as(1, GC2, None).is_empty());
    assert_eq!(fx.gc_host(GC1), Some(host));
    assert_eq!(fx.gc_host(GC2), Some(host));
    font_reply(fx.query_font(GC2));
    assert_eq!(fx.closes(host), 0);
}

/// Disconnects free GCs and font ids in either order; a font another
/// client's GC holds survives its opener, and each font closes once.
#[test]
fn disconnect_releases_fonts_in_either_order() {
    for opener_first in [true, false] {
        let mut fx = Fx::new();
        let baseline = fx.state.resources.live_font_count();
        fx.open_font_as(1, F, "cursor");
        let host = fx.host(F);
        assert!(fx.create_gc_as(1, GC1, Some(F)).is_empty());
        assert!(fx.create_gc_as(2, GC2, Some(F)).is_empty());
        let (first, second) = if opener_first { (1, 2) } else { (2, 1) };
        crate::core_loop::process_disconnect::process_disconnect(
            &mut fx.state,
            &mut fx.backend,
            ClientId(first),
        );
        assert_eq!(fx.closes(host), 0, "client {second} still holds it");
        if opener_first {
            assert_eq!(fx.gc_host(GC2), Some(host));
        }
        crate::core_loop::process_disconnect::process_disconnect(
            &mut fx.state,
            &mut fx.backend,
            ClientId(second),
        );
        assert_eq!(fx.closes(host), 1, "opener_first={opener_first}");
        assert_eq!(fx.state.resources.live_font_count(), baseline);
    }
}
