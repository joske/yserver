//! Host X11 request-side methods.
//!
//! All `HostX11Backend` methods that send wire bytes to the host (drawing,
//! extension proxies, `apply_*` sync points, etc.) live here, plus the
//! pure-byte wire builders they call (`build_xfixes_change_cursor_by_name`,
//! `build_shape_rectangles`, `patch_glyph_command_offsets`,
//! `push_card8_padded`, the font-request builders).
//!
//! Setup-time helpers (`create_window`, `create_gc`, `open_font`,
//! `map_window`) and the response framer (`read_response`,
//! `HostResponse`) stay in `mod.rs` because they're shared between the
//! constructor and these request methods. The split here is purely about
//! file size and reviewability — Rust merges all `impl HostX11Backend { ... }`
//! blocks across the module's files.

use std::io::{self, ErrorKind, Write};

use super::pump::HostStream;

use yserver_protocol::x11::{self, ClipRectangles, FontMetrics};

use crate::backend::{
    AnyHandle, CursorHandle, FontHandle, GlyphSetHandle, PictureHandle, PixmapHandle, WindowHandle,
};

use super::{
    HostClipRectangles, HostClipState, HostFillState, HostSubwindowConfig, HostSubwindowVisual,
    HostX11Backend, PointerPosition, open_font, padded_len, read_i16, read_u16, read_u32,
    write_i16, write_u16, write_u32,
};

impl HostX11Backend {
    pub fn open_font(&mut self, name: &str) -> io::Result<(FontHandle, FontMetrics)> {
        let host_xid = self.next_xid();
        let (_open_seq, _open_seq_full) = self.issue_sequence();
        write_open_font(&mut self.stream, host_xid, name.as_bytes())?;
        let (_query_seq, query_seq_full) = self.issue_sequence();
        write_query_font(&mut self.stream, host_xid)?;
        self.stream.flush()?;

        // OpenFont yields no response on success and one error on failure.
        // QueryFont always yields exactly one response (reply or error).
        // The dispatcher routes the OpenFont error (if any) through the
        // sink as an async error — we only block here on the QueryFont
        // reply. If OpenFont failed, QueryFont will *also* fail with
        // BadFont, so we still surface a useful error to the caller.
        let resp = self
            .wait_for_reply(query_seq_full)?
            .map_err(|error| error.into_io_error("QueryFont (OpenFont chain)"))?;
        let metrics = x11::parse_query_font_reply(&resp[8..]).ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                "could not parse host QueryFont reply",
            )
        })?;
        Ok((FontHandle::from_raw_panicking(host_xid), metrics))
    }

    /// Send `GetKeyboardMapping` (op 101) to the host and return
    /// `(keysyms_per_keycode, keysyms_flat)` where the keysyms slice has
    /// `count * keysyms_per_keycode` u32 values.
    pub fn get_keyboard_mapping(
        &mut self,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        let (_target, target_full) = self.issue_sequence();
        let mut out = [0u8; 8];
        out[0] = 101;
        out[2] = 2; // length in 4-byte units
        out[3] = 0;
        out[4] = first_keycode;
        out[5] = count;
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        let resp = self
            .wait_for_reply(target_full)?
            .map_err(|error| error.into_io_error("GetKeyboardMapping"))?;
        let kpc = resp[1];
        // Body bytes start at offset 32 in the response.
        let n = usize::from(count) * usize::from(kpc);
        if resp.len() < 32 + n * 4 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "GetKeyboardMapping reply truncated",
            ));
        }
        let mut keysyms = Vec::with_capacity(n);
        for i in 0..n {
            let base = 32 + i * 4;
            keysyms.push(u32::from_le_bytes([
                resp[base],
                resp[base + 1],
                resp[base + 2],
                resp[base + 3],
            ]));
        }
        Ok((kpc, keysyms))
    }

    /// Send `GetModifierMapping` (op 119) to the host and return
    /// `(keycodes_per_modifier, keycodes)` where keycodes has
    /// `8 * keycodes_per_modifier` bytes.
    pub fn get_modifier_mapping(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let (_target, target_full) = self.issue_sequence();
        let out = [119u8, 0, 1, 0];
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        let resp = self
            .wait_for_reply(target_full)?
            .map_err(|error| error.into_io_error("GetModifierMapping"))?;
        let kpm = resp[1];
        let n = 8 * usize::from(kpm);
        if resp.len() < 32 + n {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "GetModifierMapping reply truncated",
            ));
        }
        Ok((kpm, resp[32..32 + n].to_vec()))
    }

    pub fn close_font(&mut self, host_xid: u32) -> io::Result<()> {
        let mut out = Vec::new();
        out.push(46);
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.advance_sequence();
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Send `FreeCursor` (opcode 95) to the host. Two 32-bit units:
    /// the request header and the cursor XID.
    pub fn free_cursor(&mut self, host_xid: u32) -> io::Result<()> {
        let mut out = Vec::new();
        out.push(95);
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.advance_sequence();
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Send a `ListFonts` request to the host and return the full reply
    /// bytes (including the 32-byte standard reply header).
    pub fn list_fonts_proxy(&mut self, max_names: u16, pattern: &str) -> io::Result<Vec<u8>> {
        let (_target, target_full) = self.issue_sequence();
        write_list_fonts(&mut self.stream, 49, max_names, pattern.as_bytes())?;
        self.stream.flush()?;
        self.wait_for_reply(target_full)?
            .map_err(|error| error.into_io_error("ListFonts"))
    }

    /// Send a `ListFontsWithInfo` request and return all replies (one per
    /// match plus the trailing sentinel reply with name length 0).
    pub fn list_fonts_with_info_proxy(
        &mut self,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        let (_target, target_full) = self.issue_sequence();
        write_list_fonts(&mut self.stream, 50, max_names, pattern.as_bytes())?;
        self.stream.flush()?;

        let mut replies = Vec::new();
        loop {
            let resp = self
                .wait_for_reply(target_full)?
                .map_err(|error| error.into_io_error("ListFontsWithInfo"))?;
            let name_len = resp[1];
            replies.push(resp);
            if name_len == 0 {
                return Ok(replies);
            }
        }
    }
    /// Mirror the resolved bounding/clip rectangles for a top-level subwindow
    /// to the host's SHAPE extension. The local `ServerState::shape_windows`
    /// remains the source of truth for protocol-visible queries (`QueryExtents`,
    /// `GetRectangles`, `InputSelected`); this call exists so the host actually
    /// renders themed WM frames with the right silhouette.
    ///
    /// `kind` follows the SHAPE protocol enum: 0 = Bounding, 1 = Clip.
    /// No-op when the host doesn't advertise SHAPE.
    pub fn set_shape_rectangles(
        &mut self,
        host_xid: u32,
        kind: u8,
        rects: &[yserver_protocol::x11::xfixes::RegionRect],
    ) -> io::Result<()> {
        let Some(bytes) = build_shape_rectangles(self.shape_opcode, host_xid, kind, rects) else {
            return Ok(());
        };
        self.advance_sequence();
        self.stream.write_all(&bytes)?;
        self.stream.flush()
    }

    /// Forward XFIXES `ChangeCursorByName` (minor 23) to the host. The host
    /// resolves the cursor name against its own theme, so we pass through
    /// `name_bytes` verbatim. No-op when the host doesn't advertise XFIXES.
    pub fn xfixes_change_cursor_by_name(
        &mut self,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()> {
        let Some(bytes) =
            build_xfixes_change_cursor_by_name(self.xfixes_opcode, host_cursor_xid, name_bytes)
        else {
            return Ok(());
        };
        self.advance_sequence();
        self.stream.write_all(&bytes)?;
        self.stream.flush()
    }

    pub fn render_create_picture(
        &mut self,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        let Some(r) = self.render.as_ref() else {
            return Ok(None);
        };
        let host_fmt = if ynest_format == 0 {
            0u32
        } else {
            match ynest_format {
                1 => r.fmt_a1,
                2 => r.fmt_a8,
                3 => r.fmt_rgb24,
                4 => r.fmt_argb32,
                _ => 0,
            }
        };
        if host_fmt == 0 && ynest_format != 0 {
            return Ok(None);
        }
        let opcode = r.opcode;
        let host_pic = self.next_xid();
        let nvals = (values.len() / 4) as u16;
        // header(4) + pid(4) + drawable(4) + format(4) + value-mask(4) = 20 bytes = 5 units
        let length_units = 5 + nvals;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(4); // CreatePicture
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_pic);
        write_u32(&mut out, host_drawable.as_raw());
        write_u32(&mut out, host_fmt);
        write_u32(&mut out, value_mask);
        out.extend_from_slice(values);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(Some(PictureHandle::from_raw_panicking(host_pic)))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_composite(
        &mut self,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        // header(4) + op_pad(4) + src(4) + mask(4) + dst(4) + src_xy(4)
        // + mask_xy(4) + dst_xy(4) + size(4) = 36 bytes = 9 units
        self.advance_sequence();
        let mut out = Vec::with_capacity(36);
        out.push(opcode);
        out.push(8); // Composite
        write_u16(&mut out, 9);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_mask);
        write_u32(&mut out, host_dst);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        write_i16(&mut out, mask_x);
        write_i16(&mut out, mask_y);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_free_picture(&mut self, host_pic: u32) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(7); // FreePicture
        write_u16(&mut out, 2);
        write_u32(&mut out, host_pic);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_create_glyphset(
        &mut self,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        let Some(r) = self.render.as_ref() else {
            return Ok(None);
        };
        let host_fmt = match ynest_format {
            1 => r.fmt_a1,
            2 => r.fmt_a8,
            3 => r.fmt_rgb24,
            4 => r.fmt_argb32,
            _ => r.fmt_a8,
        };
        let opcode = r.opcode;
        let host_gs = self.next_xid();
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(17); // CreateGlyphSet
        write_u16(&mut out, 3);
        write_u32(&mut out, host_gs);
        write_u32(&mut out, host_fmt);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(Some(GlyphSetHandle::from_raw_panicking(host_gs)))
    }

    pub fn render_free_glyphset(&mut self, host_gs: u32) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(19); // FreeGlyphSet
        write_u16(&mut out, 2);
        write_u32(&mut out, host_gs);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_add_glyphs(&mut self, host_gs: u32, body_tail: &[u8]) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        // body_tail (already padded on the wire): num_glyphs(4) + glyph_ids + glyph_infos + glyph_data
        let padded_tail = padded_len(body_tail.len());
        let length_units = 2 + (padded_tail / 4) as u16;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(20); // AddGlyphs
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_gs);
        out.extend_from_slice(body_tail);
        out.resize(8 + padded_tail, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_free_glyphs(&mut self, host_gs: u32, glyph_ids: &[u8]) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let padded_ids = padded_len(glyph_ids.len());
        let length_units = 2 + (padded_ids / 4) as u16;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(22); // FreeGlyphs
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_gs);
        out.extend_from_slice(glyph_ids);
        out.resize(8 + padded_ids, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_composite_glyphs(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let id_size = match minor {
            23 => 1, // CompositeGlyphs8
            24 => 2, // CompositeGlyphs16
            25 => 4, // CompositeGlyphs32
            _ => 1,  // shouldn't happen, fall back to 8-bit stride
        };
        let mut patched = items.to_vec();
        patch_glyph_command_offsets(&mut patched, x_off, y_off, id_size);
        let padded_items = padded_len(patched.len());
        let length_units = 7 + (padded_items / 4) as u16;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_dst);
        write_u32(&mut out, mask_fmt);
        write_u32(&mut out, host_gs);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        out.extend_from_slice(&patched);
        out.resize(28 + padded_items, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward a RENDER request whose first body word is a picture XID.
    /// `minor` is the RENDER sub-opcode; `body` is everything after the 4-byte
    /// request header.  The first 4 bytes of `body` are replaced with
    /// `host_pic` before forwarding.
    fn render_forward_picture_op(
        &mut self,
        minor: u8,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let total = 4 + body.len();
        let length_units =
            u16::try_from(total / 4).map_err(|_| io::Error::other("RENDER request too large"))?;
        self.advance_sequence();
        let mut out = Vec::with_capacity(total);
        out.push(opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_pic); // overwrite client picture XID
        if body.len() > 4 {
            out.extend_from_slice(&body[4..]);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// RENDER::SetPictureTransform (minor=28): forward transform matrix to host.
    pub fn render_set_picture_transform(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(28, host_pic, body)
    }

    /// RENDER::SetPictureFilter (minor=30): forward filter name + values to host.
    pub fn render_set_picture_filter(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(30, host_pic, body)
    }

    /// RENDER::CreateLinearGradient (minor=34): create gradient picture on host.
    /// Allocates a fresh host picture XID and overwrites the picture-XID slot
    /// in `body` (the client body after the request header) with it.
    /// `body` is the client body after the picture XID field (p1 x/y, p2 x/y,
    /// num_stops, offsets[], colors[]).
    pub fn render_create_linear_gradient(
        &mut self,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        if self.render.is_none() {
            return Ok(None);
        }
        let host_pic = self.next_xid();
        self.render_forward_picture_op(34, host_pic, body)?;
        Ok(Some(PictureHandle::from_raw_panicking(host_pic)))
    }

    pub fn render_create_radial_gradient(
        &mut self,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        if self.render.is_none() {
            return Ok(None);
        }
        let host_pic = self.next_xid();
        self.render_forward_picture_op(35, host_pic, body)?;
        Ok(Some(PictureHandle::from_raw_panicking(host_pic)))
    }

    pub fn render_change_picture(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(5, host_pic, body)
    }

    pub fn render_set_picture_clip_rectangles(
        &mut self,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.render_forward_picture_op(6, host_pic, body)
    }

    pub fn render_create_solid_fill(
        &mut self,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        let Some(r) = self.render.as_ref() else {
            return Ok(None);
        };
        let opcode = r.opcode;
        let host_pic = self.next_xid();
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(33); // CreateSolidFill
        write_u16(&mut out, 4);
        write_u32(&mut out, host_pic);
        out.extend_from_slice(&color);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(Some(PictureHandle::from_raw_panicking(host_pic)))
    }

    pub fn render_fill_rectangles(
        &mut self,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let nrects = rects.len() / 8;
        // header(4) + op_pad(4) + dst(4) + color(8) = 20 bytes = 5 units, plus 2 units per rect
        let length_units = 5 + (nrects * 2) as u16;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(26); // FillRectangles
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_dst);
        out.extend_from_slice(&color);
        // Translate rectangles
        for rect in rects.chunks_exact(8) {
            let x = i16::from_le_bytes([rect[0], rect[1]]).wrapping_add(x_off);
            let y = i16::from_le_bytes([rect[2], rect[3]]).wrapping_add(y_off);
            let w = u16::from_le_bytes([rect[4], rect[5]]);
            let h = u16::from_le_bytes([rect[6], rect[7]]);
            write_i16(&mut out, x);
            write_i16(&mut out, y);
            write_u16(&mut out, w);
            write_u16(&mut out, h);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward RENDER::Trapezoids (minor=10) verbatim to the host.
    /// Per-trapezoid FIXED-point coords have `x_off`/`y_off` (in pixels)
    /// added so child-window pictures land at the right offset on the
    /// shared host top-level. For top-level pictures the offsets are 0
    /// and this is a straight passthrough.
    pub fn render_trapezoids(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let n_traps = traps.len() / 40;
        // header(4) + op_pad(4) + src(4) + dst(4) + mask_format(4)
        // + src_xy(4) = 24 bytes = 6 units, plus 10 units (40 bytes) per trap
        let length_units = 6u16 + (n_traps as u16) * 10;
        self.advance_sequence();
        let mut out = Vec::with_capacity(24 + n_traps * 40);
        out.push(opcode);
        out.push(10); // Trapezoids
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_dst);
        write_u32(&mut out, host_mask_format);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);

        // FIXED is i32 with 16 bits of fraction; integer offset is
        // shifted left by 16 before adding.
        let dx = (i32::from(x_off)) << 16;
        let dy = (i32::from(y_off)) << 16;
        for trap in traps.chunks_exact(40) {
            // Y-coords at offsets 0, 4, 12, 20, 28, 36
            // X-coords at offsets 8, 16, 24, 32
            let mut t = [0u8; 40];
            t.copy_from_slice(trap);
            let patch = |t: &mut [u8; 40], off: usize, delta: i32| {
                let v = i32::from_le_bytes([t[off], t[off + 1], t[off + 2], t[off + 3]])
                    .wrapping_add(delta);
                t[off..off + 4].copy_from_slice(&v.to_le_bytes());
            };
            for &y_off_in_trap in &[0usize, 4, 12, 20, 28, 36] {
                patch(&mut t, y_off_in_trap, dy);
            }
            for &x_off_in_trap in &[8usize, 16, 24, 32] {
                patch(&mut t, x_off_in_trap, dx);
            }
            out.extend_from_slice(&t);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward RENDER::Triangles (minor=11), TriStrip (12), TriFan (13)
    /// verbatim to the host. Per-point FIXED-point coords have
    /// `x_off`/`y_off` (in pixels) added so child-window pictures land
    /// at the right offset on the shared host top-level. For top-level
    /// pictures the offsets are 0 and this is a straight passthrough.
    #[allow(clippy::too_many_arguments)]
    pub fn render_triangles_op(
        &mut self,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        primitives: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;

        // Per-element size: 24 bytes for Triangles (3 XPointFixed),
        // 8 bytes for TriStrip / TriFan (single XPointFixed).
        let elem_size = match minor {
            11 => 24,
            12 | 13 => 8,
            _ => return Ok(()),
        };
        if !primitives.len().is_multiple_of(elem_size) {
            return Ok(());
        }

        // header(4) + op_pad(4) + src(4) + dst(4) + mask_format(4)
        // + src_xy(4) = 24 bytes = 6 units.
        let length_units = 6u16 + ((primitives.len() / 4) as u16);
        self.advance_sequence();
        let mut out = Vec::with_capacity(24 + primitives.len());
        out.push(opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_dst);
        write_u32(&mut out, host_mask_format);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);

        // FIXED is i32 with 16 bits of fraction; integer offset is
        // shifted left by 16 before adding.
        let dx = (i32::from(x_off)) << 16;
        let dy = (i32::from(y_off)) << 16;
        // X offsets within each element (every PointFixed starts on an
        // 8-byte boundary; X is the first i32, Y the second).
        let (x_offs, y_offs): (&[usize], &[usize]) = match minor {
            11 => (&[0, 8, 16], &[4, 12, 20]),
            12 | 13 => (&[0], &[4]),
            _ => unreachable!(),
        };
        for elem in primitives.chunks_exact(elem_size) {
            let mut e = vec![0u8; elem_size];
            e.copy_from_slice(elem);
            let patch = |buf: &mut [u8], off: usize, delta: i32| {
                let v = i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
                    .wrapping_add(delta);
                buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
            };
            for &xo in x_offs {
                patch(&mut e, xo, dx);
            }
            for &yo in y_offs {
                patch(&mut e, yo, dy);
            }
            out.extend_from_slice(&e);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_query_version(&mut self) -> io::Result<(u32, u32)> {
        let Some(r) = self.render.as_ref() else {
            return Ok((0, 0));
        };
        let opcode = r.opcode;
        let (_seq, seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0); // QueryVersion
        write_u16(&mut out, 3);
        write_u32(&mut out, 0); // client major
        write_u32(&mut out, 11); // client minor
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        let resp = self
            .wait_for_reply(seq_full)?
            .map_err(|error| error.into_io_error("RENDER QueryVersion"))?;
        let major = read_u32(&resp[8..12]);
        let minor = read_u32(&resp[12..16]);
        Ok((major, minor))
    }

    pub fn xkb_proxy(&mut self, minor: u8, body: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let Some(xkb_opcode) = self.xkb.as_ref().map(|xkb| xkb.opcode) else {
            return Err(io::Error::other("XKB not available on host"));
        };
        let (_target, target_full) = self.issue_sequence();

        // Standard X11 request: major(1) minor(1) length(2)
        let total_len = body.len() + 4;
        let length_units = u16::try_from(total_len / 4)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "XKB request too large"))?;

        let mut out = Vec::with_capacity(total_len);
        out.push(xkb_opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        out.extend_from_slice(body);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        if !xkb_minor_has_reply(minor) {
            return Ok(None);
        }

        Ok(Some(
            self.wait_for_reply(target_full)?
                .map_err(|error| error.into_io_error("XKB proxy"))?,
        ))
    }

    pub fn ping(&mut self) -> io::Result<()> {
        self.advance_sequence();
        self.stream.write_all(&[127, 0, 1, 0])
    }

    pub fn query_pointer(&mut self) -> io::Result<PointerPosition> {
        let (_target, target_full) = self.issue_sequence();

        let mut out = Vec::new();
        out.push(38);
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, self.window_id);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let reply = self
            .wait_for_reply(target_full)?
            .map_err(|error| error.into_io_error("QueryPointer"))?;

        Ok(PointerPosition {
            same_screen: reply[1] != 0,
            win_x: read_i16(&reply[20..22]),
            win_y: read_i16(&reply[22..24]),
            mask: read_u16(&reply[24..26]),
            host_xid: None,
        })
    }

    /// Forward `XCreateWindow(host_parent, host_xid, x, y, width, height,
    /// border_width, ...)` fire-and-forget. No reply is awaited — host
    /// errors (BadAlloc / BadMatch / BadValue) become async and are
    /// absorbed silently by `read_response` drain loops elsewhere. The
    /// caller is responsible for pre-validating visual / depth / parent
    /// locally so we don't dangle a host xid that the host never
    /// allocated.
    ///
    /// `event_mask = 0` always — pointer/exposure events are selected
    /// later via `Backend::register_top_level` (top-levels only during
    /// Phase 3.6 Step 2; sub-window mirroring stays dormant).
    pub fn create_subwindow(
        &mut self,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle> {
        let host_xid = self.next_xid();
        let host_parent = host_parent.as_raw();
        let mut out = Vec::new();
        // Subwindow value list, in increasing value-mask bit order:
        // - bit 0: background-pixmap. If set, host auto-clears to this
        //   pixmap on Expose / map. Takes precedence over bit 1.
        // - bit 1: background-pixel. If set (and bit 0 isn't), host
        //   auto-clears to this solid colour. Required so the bg shows
        //   up without the client re-filling on every Expose.
        // - bit 4: bit-gravity = NorthWest, preserve NW pixels on
        //   resize.
        // - bit 6: backing-store = Always, host keeps a backing pixmap
        //   so partial occlusion doesn't blank our content.
        // - bit 13: colormap; only when visual differs from parent
        //   (Explicit).
        let needs_colormap = matches!(visual, HostSubwindowVisual::Explicit { .. });
        let mut value_mask: u32 = (1 << 4) | (1 << 6);
        if background_pixmap.is_some() {
            value_mask |= 1 << 0;
        }
        if background_pixel.is_some() && background_pixmap.is_none() {
            value_mask |= 1 << 1;
        }
        if needs_colormap {
            value_mask |= 1 << 13;
        }
        let value_count: u16 = u16::try_from(value_mask.count_ones()).unwrap_or(2);
        out.push(1); // CreateWindow opcode
        out.push(visual.depth());
        write_u16(&mut out, 8 + value_count); // length: 8 fixed + value words
        write_u32(&mut out, host_xid);
        write_u32(&mut out, host_parent);
        write_i16(&mut out, x);
        write_i16(&mut out, y);
        let safe_width = width.max(1);
        let safe_height = height.max(1);
        write_u16(&mut out, safe_width);
        write_u16(&mut out, safe_height);
        write_u16(&mut out, border_width);
        write_u16(&mut out, 0); // class = CopyFromParent
        write_u32(&mut out, visual.visual_xid());
        write_u32(&mut out, value_mask);
        if let Some(pix) = background_pixmap {
            write_u32(&mut out, pix);
        }
        if let Some(pix) = background_pixel
            && background_pixmap.is_none()
        {
            write_u32(&mut out, pix);
        }
        write_u32(&mut out, 1); // bit-gravity = NorthWest
        write_u32(&mut out, 2); // backing-store = Always
        if let HostSubwindowVisual::Explicit { colormap_xid, .. } = visual {
            write_u32(&mut out, colormap_xid);
        }
        self.stream.write_all(&out)?;
        self.advance_sequence();
        // Phase 6.3 Step 6: the cross-connection sync fence
        // (`sync_main_connection`) is gone. With the merged main
        // connection, the follow-up `ChangeWindowAttributes` selecting
        // `ExposureMask` on this xid travels on the same socket as
        // this `CreateWindow` — wire ordering is naturally sequential,
        // so the host can never see CWA before the create.
        log::debug!(
            "create_subwindow: host_xid=0x{:x} parent=0x{:x} pos=({x},{y}) size={width}x{height} bw={border_width} bg_pixel={background_pixel:?} bg_pixmap={background_pixmap:?}",
            host_xid,
            host_parent,
        );
        self.host_drawable_depths.insert(host_xid, visual.depth());
        Ok(WindowHandle::from_raw_panicking(host_xid))
    }

    pub fn destroy_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(4); // DestroyWindow
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.host_drawable_depths.remove(&host_xid);
        Ok(())
    }

    #[allow(
        dead_code,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::cast_lossless
    )]
    // Sign-extension of i16 → u32 is intentional per X11 wire format (INT16
    // in a CARD32 slot must be sign-extended).
    pub fn configure_subwindow(
        &mut self,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let mut value_mask: u16 = 0;
        let mut values: Vec<u8> = Vec::new();
        if let Some(x) = config.x {
            value_mask |= 1 << 0;
            write_u32(&mut values, x as i32 as u32);
        }
        if let Some(y) = config.y {
            value_mask |= 1 << 1;
            write_u32(&mut values, y as i32 as u32);
        }
        if let Some(width) = config.width {
            value_mask |= 1 << 2;
            write_u32(&mut values, u32::from(width.max(1)));
        }
        if let Some(height) = config.height {
            value_mask |= 1 << 3;
            write_u32(&mut values, u32::from(height.max(1)));
        }
        if let Some(border_width) = config.border_width {
            value_mask |= 1 << 4;
            write_u32(&mut values, u32::from(border_width));
        }
        if let Some(sibling) = config.sibling {
            value_mask |= 1 << 5;
            write_u32(&mut values, sibling);
        }
        if let Some(stack_mode) = config.stack_mode {
            value_mask |= 1 << 6;
            write_u32(&mut values, u32::from(stack_mode));
        }
        if value_mask == 0 {
            return Ok(());
        }

        let length_units = 3 + u16::try_from(values.len() / 4).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "too many ConfigureWindow values")
        })?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(12); // ConfigureWindow
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u16(&mut out, value_mask);
        write_u16(&mut out, 0); // pad
        out.extend_from_slice(&values);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward `XReparentWindow(host_xid, host_parent, x, y)` to the
    /// host so the host tree stays in sync with the local logical tree.
    /// Used by nested.rs's ReparentWindow handler whenever a window
    /// moves between local parents and at least one of those parents
    /// has a real host-mirrored child window. Fire-and-forget.
    pub fn reparent_subwindow(
        &mut self,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        // Wire layout: opcode(1) pad(1) length(2 = 4) window(4) parent(4)
        //              x(2) y(2)
        let mut out = Vec::with_capacity(16);
        out.push(7); // ReparentWindow
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, host_parent);
        write_i16(&mut out, x);
        write_i16(&mut out, y);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(())
    }

    /// Forward a `ChangeWindowAttributes` value-list to the host child
    /// of a sub-window so attribute updates after CreateWindow (bg
    /// pixel changes, cursor, etc.) take effect on the host. Caller
    /// builds the `(value_mask, values)` pair following X11
    /// CreateWindow semantics. Fire-and-forget; host BadValue /
    /// BadMatch is absorbed silently.
    pub fn change_subwindow_attributes(
        &mut self,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        if value_mask == 0 || values.is_empty() {
            return Ok(());
        }
        let length_units = 3 + u16::try_from(values.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "too many CWA values"))?;
        let mut out = Vec::with_capacity(usize::from(length_units) * 4);
        out.push(2); // ChangeWindowAttributes opcode
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, value_mask);
        for v in values {
            write_u32(&mut out, *v);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(())
    }

    /// Set the host container window's background pixmap so the host
    /// server auto-fills regions uncovered when nested top-levels move,
    /// without us needing to plumb Expose events through the nested root.
    /// Pass `None` (or 0) for `None` (no bg pixmap → window stays its
    /// previous content).
    pub fn set_container_background_pixmap(&mut self, host_pixmap_xid: u32) -> io::Result<()> {
        // ChangeWindowAttributes (opcode 2): window(4) value-mask(4) values(4*n)
        // value-mask CWBackPixmap = 0x00000001
        self.advance_sequence();
        let mut out = Vec::with_capacity(16);
        out.push(2);
        out.push(0);
        write_u16(&mut out, 4); // length 4 = 16 bytes
        write_u32(&mut out, self.window_id);
        write_u32(&mut out, 0x0000_0001); // CWBackPixmap
        write_u32(&mut out, host_pixmap_xid);
        self.stream.write_all(&out)?;

        // Force the host to repaint immediately so the new bg shows up
        // even if nothing has triggered an Expose yet. ClearArea(window,
        // 0,0,0,0, false) clears the entire window.
        self.advance_sequence();
        let mut clear = Vec::with_capacity(16);
        clear.push(61); // ClearArea
        clear.push(0); // exposures = false
        write_u16(&mut clear, 4); // length 4 = 16 bytes
        write_u32(&mut clear, self.window_id);
        write_i16(&mut clear, 0);
        write_i16(&mut clear, 0);
        write_u16(&mut clear, 0);
        write_u16(&mut clear, 0);
        self.stream.write_all(&clear)?;
        self.stream.flush()
    }

    pub fn set_container_background_pixel(&mut self, pixel: u32) -> io::Result<()> {
        // ChangeWindowAttributes with value-mask CWBackPixel = 0x00000002.
        // Setting bg-pixel makes the host auto-clear the container with a
        // solid color whenever a region is exposed (e.g. a top-level
        // subwindow is moved across it). Without this, drags leave trails of
        // stale window content on the desktop.
        self.advance_sequence();
        let mut out = Vec::with_capacity(16);
        out.push(2);
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, self.window_id);
        write_u32(&mut out, 0x0000_0002); // CWBackPixel
        write_u32(&mut out, pixel);
        self.stream.write_all(&out)?;

        // ClearArea so the new color is visible immediately.
        self.advance_sequence();
        let mut clear = Vec::with_capacity(16);
        clear.push(61);
        clear.push(0);
        write_u16(&mut clear, 4);
        write_u32(&mut clear, self.window_id);
        write_i16(&mut clear, 0);
        write_i16(&mut clear, 0);
        write_u16(&mut clear, 0);
        write_u16(&mut clear, 0);
        self.stream.write_all(&clear)?;
        self.stream.flush()
    }

    pub fn clear_area(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        exposures: bool,
    ) -> io::Result<()> {
        self.advance_sequence();
        let mut clear = Vec::with_capacity(16);
        clear.push(61);
        clear.push(u8::from(exposures));
        write_u16(&mut clear, 4);
        write_u32(&mut clear, host_xid);
        write_i16(&mut clear, x);
        write_i16(&mut clear, y);
        write_u16(&mut clear, width);
        write_u16(&mut clear, height);
        self.stream.write_all(&clear)?;
        self.stream.flush()
    }

    pub fn map_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(8); // MapWindow
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn unmap_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(10); // UnmapWindow
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn warp_pointer(&mut self, dst_host_xid: u32, dst_x: i16, dst_y: i16) -> io::Result<()> {
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(41); // WarpPointer
        out.push(0);
        write_u16(&mut out, 6); // length: 6 units = 24 bytes
        write_u32(&mut out, 0); // src_window = None
        write_u32(&mut out, dst_host_xid);
        write_i16(&mut out, 0); // src_x
        write_i16(&mut out, 0); // src_y
        write_u16(&mut out, 0); // src_width
        write_u16(&mut out, 0); // src_height
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward `GetAtomName(atom)` to the host and return its name, or
    /// `Ok(None)` if the host doesn't know the atom (returns `BadAtom`).
    /// Used to resolve atoms that leak in via host-proxied replies — most
    /// notably the `FONTPROP` atoms in `ListFontsWithInfo` payloads.
    pub fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>> {
        let (_target, target_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(17u8); // GetAtomName
        out.push(0);
        write_u16(&mut out, 2); // length: 2 units = 8 bytes
        write_u32(&mut out, atom);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        let resp = match self.wait_for_reply(target_full)? {
            Ok(reply) => reply,
            Err(error) if error.code == x11::error::BAD_ATOM => return Ok(None),
            Err(error) => return Err(error.into_io_error("GetAtomName")),
        };
        let name_len = u16::from_le_bytes([resp[8], resp[9]]) as usize;
        let name_start = 32usize;
        let end = name_start.checked_add(name_len).ok_or_else(|| {
            io::Error::new(ErrorKind::InvalidData, "GetAtomName name length overflow")
        })?;
        if resp.len() < end {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "GetAtomName reply truncated",
            ));
        }
        let name = String::from_utf8_lossy(&resp[name_start..end]).into_owned();
        Ok(Some(name))
    }

    pub fn create_pixmap(
        &mut self,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        let host_xid = self.next_xid();
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(53); // CreatePixmap opcode
        out.push(depth);
        write_u16(&mut out, 4); // length: 4 units = 16 bytes
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.window_id); // screen-compatible drawable
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.host_drawable_depths.insert(host_xid, depth);
        Ok(PixmapHandle::from_raw_panicking(host_xid))
    }

    pub fn free_pixmap(&mut self, host_xid: u32) -> io::Result<()> {
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(54); // FreePixmap opcode
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.host_drawable_depths.remove(&host_xid);
        Ok(())
    }

    pub fn create_cursor(
        &mut self,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        let cursor_xid = self.next_xid();
        self.advance_sequence();
        let mut buf = Vec::with_capacity(32);
        buf.push(93u8);
        buf.push(0u8);
        write_u16(&mut buf, 8u16);
        write_u32(&mut buf, cursor_xid);
        write_u32(&mut buf, source_pixmap.as_raw());
        write_u32(&mut buf, mask_pixmap.map(|h| h.as_raw()).unwrap_or(0));
        write_u16(&mut buf, fore.0);
        write_u16(&mut buf, fore.1);
        write_u16(&mut buf, fore.2);
        write_u16(&mut buf, back.0);
        write_u16(&mut buf, back.1);
        write_u16(&mut buf, back.2);
        write_u16(&mut buf, hot_x);
        write_u16(&mut buf, hot_y);
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        Ok(CursorHandle::from_raw_panicking(cursor_xid))
    }

    pub fn create_glyph_cursor(
        &mut self,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        let cursor_xid = self.next_xid();
        self.advance_sequence();
        let mut buf = Vec::with_capacity(32);
        buf.push(94u8); // CreateGlyphCursor
        buf.push(0u8);
        write_u16(&mut buf, 8u16);
        write_u32(&mut buf, cursor_xid);
        write_u32(&mut buf, source_font.as_raw());
        write_u32(&mut buf, mask_font.map(|h| h.as_raw()).unwrap_or(0));
        write_u16(&mut buf, source_char);
        write_u16(&mut buf, mask_char);
        write_u16(&mut buf, fore.0);
        write_u16(&mut buf, fore.1);
        write_u16(&mut buf, fore.2);
        write_u16(&mut buf, back.0);
        write_u16(&mut buf, back.1);
        write_u16(&mut buf, back.2);
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        Ok(CursorHandle::from_raw_panicking(cursor_xid))
    }

    pub fn render_create_cursor(
        &mut self,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        let Some(r) = self.render.as_ref() else {
            return Ok(None);
        };
        let opcode = r.opcode;
        let cursor_xid = self.next_xid();
        self.advance_sequence();
        let mut buf = Vec::with_capacity(16);
        buf.push(opcode);
        buf.push(27); // CreateCursor minor opcode
        write_u16(&mut buf, 4u16);
        write_u32(&mut buf, cursor_xid);
        write_u32(&mut buf, host_src_pic.as_raw());
        write_u16(&mut buf, x);
        write_u16(&mut buf, y);
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        Ok(Some(CursorHandle::from_raw_panicking(cursor_xid)))
    }

    /// Install a cursor on a host window. X11 has no standalone DefineCursor
    /// request (Xlib's `XDefineCursor` is implemented via ChangeWindowAttributes
    /// with the CWCursor value-mask bit); we emit ChangeWindowAttributes
    /// directly. Pass `cursor_host_xid = 0` to clear the cursor (X11 None).
    pub fn define_cursor(&mut self, host_window_xid: u32, cursor_host_xid: u32) -> io::Result<()> {
        const CW_CURSOR: u32 = 0x0000_4000;
        self.advance_sequence();
        let mut buf = Vec::with_capacity(16);
        buf.push(2u8); // ChangeWindowAttributes
        buf.push(0u8); // unused
        write_u16(&mut buf, 4u16); // request length in 4-byte units
        write_u32(&mut buf, host_window_xid);
        write_u32(&mut buf, CW_CURSOR);
        write_u32(&mut buf, cursor_host_xid);
        self.stream.write_all(&buf)?;
        self.stream.flush()
    }

    pub fn set_clip_rectangles(&mut self, clip: Option<ClipRectangles>) -> io::Result<()> {
        let new_state = match clip {
            Some(clip) => HostClipState::Rectangles(HostClipRectangles {
                ordering: clip.ordering,
                x_origin: clip.x_origin,
                y_origin: clip.y_origin,
                rectangles: clip.rectangles,
            }),
            None => HostClipState::None,
        };
        self.set_host_clip(new_state)
    }

    pub fn clear_clip_rectangles(&mut self) -> io::Result<()> {
        self.set_host_clip(HostClipState::None)
    }

    /// Set the host shared GC's `clip-mask` to a host pixmap, shifted
    /// by `(x_offset + clip_x_origin, y_offset + clip_y_origin)`. Used
    /// when forwarding a client `ChangeGC(clip_mask=Pixmap)` so the
    /// host clips drawing to the 1-bits of the depth-1 mask pixmap.
    pub fn set_clip_pixmap(
        &mut self,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        self.set_host_clip(HostClipState::Pixmap {
            host_pixmap,
            x_origin: clip_x_origin,
            y_origin: clip_y_origin,
        })
    }

    fn set_host_clip(&mut self, new_state: HostClipState) -> io::Result<()> {
        if self.current_clip == new_state {
            return Ok(());
        }

        match &new_state {
            HostClipState::Rectangles(clip) => {
                let length_units = 3 + u16::try_from(clip.rectangles.len() / 4).map_err(|_| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        "too many clip rectangles for one X11 request",
                    )
                })?;
                self.advance_sequence();
                let mut out = Vec::new();
                out.push(59); // SetClipRectangles
                out.push(clip.ordering);
                write_u16(&mut out, length_units);
                write_u32(&mut out, self.gc_id);
                write_i16(&mut out, clip.x_origin);
                write_i16(&mut out, clip.y_origin);
                out.extend_from_slice(&clip.rectangles);
                self.stream.write_all(&out)?;
            }
            HostClipState::Pixmap {
                host_pixmap,
                x_origin,
                y_origin,
            } => {
                // Single ChangeGC with three components: clip_x_origin
                // (1<<17) + clip_y_origin (1<<18) + clip-mask (1<<19).
                self.advance_sequence();
                let mut out = Vec::new();
                out.push(56); // ChangeGC
                out.push(0);
                write_u16(&mut out, 6); // header(4) + mask(4) + 3 values(12) = 24 bytes = 6 units
                write_u32(&mut out, self.gc_id);
                write_u32(&mut out, (1 << 17) | (1 << 18) | (1 << 19));
                write_i16(&mut out, *x_origin);
                write_u16(&mut out, 0); // pad to 4 bytes
                write_i16(&mut out, *y_origin);
                write_u16(&mut out, 0); // pad
                write_u32(&mut out, *host_pixmap);
                self.stream.write_all(&out)?;
            }
            HostClipState::None => {
                self.advance_sequence();
                let mut out = Vec::new();
                out.push(56); // ChangeGC
                out.push(0);
                write_u16(&mut out, 4);
                write_u32(&mut out, self.gc_id);
                write_u32(&mut out, 1 << 19); // clip-mask
                write_u32(&mut out, 0); // None
                self.stream.write_all(&out)?;
            }
        }

        self.current_clip = new_state;
        self.stream.flush()
    }

    /// Set the host shared GC to `fill-style=Tiled, tile=host_pixmap` with
    /// the given tile origin. Per X11 spec, ChangeGC tile/x-origin/y-origin
    /// each occupy 4 bytes (i16 pad-to-u32). e16 calls this before
    /// PolyFillRectangle to tile a theme pixmap onto popup backgrounds.
    pub fn set_gc_fill_tiled(
        &mut self,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        let new_state = HostFillState::Tiled {
            host_pixmap,
            x_origin: tile_x_origin,
            y_origin: tile_y_origin,
        };
        if self.current_fill == new_state {
            return Ok(());
        }
        // Single ChangeGC: fill-style (1<<8) + tile (1<<10) +
        // tile-stipple-x-origin (1<<12) + tile-stipple-y-origin (1<<13).
        self.advance_sequence();
        let mut out = Vec::with_capacity(28);
        out.push(56); // ChangeGC
        out.push(0);
        // header(4) + gc(4) + mask(4) + 4 values × 4 bytes = 28 = 7 units
        write_u16(&mut out, 7);
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13));
        // fill-style = Tiled (1) — value is 1 byte but X11 GC values are
        // CARD32-padded.
        out.push(1);
        out.push(0);
        out.push(0);
        out.push(0);
        write_u32(&mut out, host_pixmap);
        write_i16(&mut out, tile_x_origin);
        write_u16(&mut out, 0); // pad
        write_i16(&mut out, tile_y_origin);
        write_u16(&mut out, 0); // pad
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.current_fill = new_state;
        Ok(())
    }

    /// Push the `DrawState`'s clip-state to the host shared GC.
    /// Honours `ChangeGC(clip_mask=Pixmap)` (wmaker decoration symbols)
    /// — without this, depth-1 clip-mask draws fill the entire rect with
    /// the foreground colour and X/dot symbols vanish.
    pub fn apply_clip_state(&mut self, clip: &crate::backend::ClipState) -> io::Result<()> {
        match clip {
            crate::backend::ClipState::Rectangles { rects, .. } => {
                self.set_clip_rectangles(Some(rects.clone()))
            }
            crate::backend::ClipState::Pixmap { origin, pixmap } => {
                self.set_clip_pixmap(pixmap.as_raw(), origin.0, origin.1)
            }
            crate::backend::ClipState::None => self.clear_clip_rectangles(),
        }
    }

    /// Push the `DrawState`'s fill-state to the host shared GC.
    /// Stippled / OpaqueStippled fall through to Solid for now; the
    /// caller is expected to reset to Solid after a fill draw so an
    /// unrelated subsequent draw doesn't inherit the tile.
    pub fn apply_fill_state(&mut self, fill: &crate::backend::FillState) -> io::Result<()> {
        match fill {
            crate::backend::FillState::Tiled { pixmap, origin } => {
                self.set_gc_fill_tiled(pixmap.as_raw(), origin.0, origin.1)
            }
            _ => self.set_gc_fill_solid(),
        }
    }

    /// Push the GC attributes that aren't already covered by the
    /// foreground / clip / fill helpers (function, plane-mask,
    /// line-width / style / cap / join, fill-rule, subwindow-mode,
    /// graphics-exposures, dash-offset, dashes, arc-mode) to the host's
    /// shared GC. Called by drawing call sites after `apply_clip_state`
    /// / `apply_fill_state` so the host honours the full GC state, not
    /// just clip + fill + foreground.
    ///
    /// All fields are sent in a single ChangeGC, with the value-mask
    /// limited to the fields that have changed since the last call.
    /// This is the additive-scope behavioural improvement of Phase 6.2:
    /// pre-Phase-6.2, fields like `function=Xor` were silently
    /// overridden to `Copy` because we never forwarded them.
    pub fn apply_draw_state(&mut self, state: &crate::backend::DrawState) -> io::Result<()> {
        let mut mask: u32 = 0;
        let mut values: Vec<u8> = Vec::new();

        let function_byte = state.function.protocol_value();
        if self.current_function != Some(function_byte) {
            mask |= 1 << 0;
            push_card8_padded(&mut values, function_byte);
        }
        if self.current_plane_mask != Some(state.plane_mask) {
            mask |= 1 << 1;
            write_u32(&mut values, state.plane_mask);
        }
        if self.current_line_width != Some(state.line_width) {
            mask |= 1 << 4;
            write_u16(&mut values, state.line_width);
            write_u16(&mut values, 0); // pad to 4 bytes
        }
        let line_style_byte = state.line_style.protocol_value();
        if self.current_line_style != Some(line_style_byte) {
            mask |= 1 << 5;
            push_card8_padded(&mut values, line_style_byte);
        }
        let cap_style_byte = state.cap_style.protocol_value();
        if self.current_cap_style != Some(cap_style_byte) {
            mask |= 1 << 6;
            push_card8_padded(&mut values, cap_style_byte);
        }
        let join_style_byte = state.join_style.protocol_value();
        if self.current_join_style != Some(join_style_byte) {
            mask |= 1 << 7;
            push_card8_padded(&mut values, join_style_byte);
        }
        let fill_rule_byte = state.fill_rule.protocol_value();
        if self.current_fill_rule != Some(fill_rule_byte) {
            mask |= 1 << 9;
            push_card8_padded(&mut values, fill_rule_byte);
        }
        let submode_byte = state.subwindow_mode.protocol_value();
        if self.current_subwindow_mode != Some(submode_byte) {
            mask |= 1 << 15;
            push_card8_padded(&mut values, submode_byte);
        }
        if self.current_graphics_exposures != Some(state.graphics_exposures) {
            mask |= 1 << 16;
            push_card8_padded(&mut values, u8::from(state.graphics_exposures));
        }
        if self.current_dash_offset != Some(state.dash_offset) {
            mask |= 1 << 20;
            write_i16(&mut values, state.dash_offset);
            write_u16(&mut values, 0); // pad
        }
        // CPDashList: a single byte (the on/off length). We store the
        // pair as `[n, n]`; if the GC has a different shape (set via a
        // hypothetical SetDashes opcode), we send the first byte.
        let dash_byte = *state.dashes.first().unwrap_or(&4);
        if self
            .current_dashes
            .as_ref()
            .is_none_or(|d| d.first() != Some(&dash_byte))
        {
            mask |= 1 << 21;
            push_card8_padded(&mut values, dash_byte);
        }
        let arc_byte = state.arc_mode.protocol_value();
        if self.current_arc_mode != Some(arc_byte) {
            mask |= 1 << 22;
            push_card8_padded(&mut values, arc_byte);
        }

        if mask == 0 {
            return Ok(());
        }

        // Header(4) + gc(4) + mask(4) + values = 12 + values.len() bytes.
        // Length is in 4-byte units.
        let length_bytes = 12 + values.len();
        debug_assert!(length_bytes.is_multiple_of(4));
        let length_units = u16::try_from(length_bytes / 4).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "apply_draw_state: too many fields")
        })?;
        self.advance_sequence();
        let mut out = Vec::with_capacity(length_bytes);
        out.push(56); // ChangeGC
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, mask);
        out.extend_from_slice(&values);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        // Update cached state on success.
        if mask & (1 << 0) != 0 {
            self.current_function = Some(function_byte);
        }
        if mask & (1 << 1) != 0 {
            self.current_plane_mask = Some(state.plane_mask);
        }
        if mask & (1 << 4) != 0 {
            self.current_line_width = Some(state.line_width);
        }
        if mask & (1 << 5) != 0 {
            self.current_line_style = Some(line_style_byte);
        }
        if mask & (1 << 6) != 0 {
            self.current_cap_style = Some(cap_style_byte);
        }
        if mask & (1 << 7) != 0 {
            self.current_join_style = Some(join_style_byte);
        }
        if mask & (1 << 9) != 0 {
            self.current_fill_rule = Some(fill_rule_byte);
        }
        if mask & (1 << 15) != 0 {
            self.current_subwindow_mode = Some(submode_byte);
        }
        if mask & (1 << 16) != 0 {
            self.current_graphics_exposures = Some(state.graphics_exposures);
        }
        if mask & (1 << 20) != 0 {
            self.current_dash_offset = Some(state.dash_offset);
        }
        if mask & (1 << 21) != 0 {
            self.current_dashes = Some(state.dashes.clone());
        }
        if mask & (1 << 22) != 0 {
            self.current_arc_mode = Some(arc_byte);
        }
        Ok(())
    }

    /// Reset the host shared GC's fill-style to Solid. No-op if already
    /// Solid. Called after every fill draw that flipped to Tiled, so
    /// subsequent unrelated draws on the shared GC don't inherit the tile.
    pub fn set_gc_fill_solid(&mut self) -> io::Result<()> {
        if self.current_fill == HostFillState::Solid {
            return Ok(());
        }
        self.advance_sequence();
        let mut out = Vec::with_capacity(16);
        out.push(56); // ChangeGC
        out.push(0);
        write_u16(&mut out, 4); // gc(4) + mask(4) + fill_style(4) = 12 + header(4) = 16 bytes = 4 units
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, 1 << 8); // fill-style only
        out.push(0); // Solid
        out.push(0);
        out.push(0);
        out.push(0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.current_fill = HostFillState::Solid;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_area(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let gc = self.pick_gc(dst_host_xid)?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(62); // CopyArea opcode
        out.push(0);
        write_u16(&mut out, 7); // length: 7 units = 28 bytes
        write_u32(&mut out, src_host_xid);
        write_u32(&mut out, dst_host_xid);
        write_u32(&mut out, gc);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_plane(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        let gc = self.pick_gc(dst_host_xid)?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(63); // CopyPlane opcode
        out.push(0);
        write_u16(&mut out, 8); // length: 8 units = 32 bytes
        write_u32(&mut out, src_host_xid);
        write_u32(&mut out, dst_host_xid);
        write_u32(&mut out, gc);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        write_u32(&mut out, plane);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    /// Send GetImage to the host and return the raw reply bytes (32-byte header
    /// + image data).  Returns None on host error or if the region is invalid.
    pub fn get_image(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        let (_target_seq, target_seq_full) = self.issue_sequence();

        let mut out = [0u8; 20];
        out[0] = 73; // GetImage
        out[1] = format;
        out[2] = 5;
        out[3] = 0; // request length = 5 words = 20 bytes
        out[4..8].copy_from_slice(&host_xid.to_le_bytes());
        out[8..10].copy_from_slice(&x.to_le_bytes());
        out[10..12].copy_from_slice(&y.to_le_bytes());
        out[12..14].copy_from_slice(&width.to_le_bytes());
        out[14..16].copy_from_slice(&height.to_le_bytes());
        out[16..20].copy_from_slice(&plane_mask.to_le_bytes());
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        match self.wait_for_reply(target_seq_full)? {
            Ok(reply) => Ok(Some(reply)),
            Err(_) => Ok(None),
        }
    }

    /// Pick a host GC whose root+depth match destination `host_xid`. For
    /// untracked or depth-24 drawables, returns the default `gc_id`
    /// (which is depth-24). Otherwise creates and caches a per-depth GC.
    /// Using the depth-24 `gc_id` against e.g. a depth-32 ARGB pixmap
    /// would BadMatch on the host and silently drop the request — see
    /// the depth-32 functest probe in xts bucket 1.
    fn pick_gc(&mut self, host_xid: u32) -> io::Result<u32> {
        let depth = self
            .host_drawable_depths
            .get(&host_xid)
            .copied()
            .unwrap_or(24);
        if depth == 24 {
            return Ok(self.gc_id);
        }
        self.ensure_gc_for_depth(depth, host_xid)
    }

    /// As `pick_gc`, but also ensures the picked GC's foreground is `fg`.
    /// For depth-24 the cached `change_foreground` fast path is used; for
    /// other depths we always emit a ChangeGC since we don't cache per-
    /// depth-GC foreground state (depth-32 fills are rare in practice).
    fn pick_gc_with_foreground(&mut self, host_xid: u32, fg: u32) -> io::Result<u32> {
        let depth = self
            .host_drawable_depths
            .get(&host_xid)
            .copied()
            .unwrap_or(24);
        if depth == 24 {
            if self.current_foreground != fg {
                self.change_foreground(fg)?;
            }
            return Ok(self.gc_id);
        }
        let gc = self.ensure_gc_for_depth(depth, host_xid)?;
        self.write_gc_foreground(gc, fg)?;
        Ok(gc)
    }

    fn write_gc_foreground(&mut self, gc: u32, fg: u32) -> io::Result<()> {
        self.advance_sequence();
        let mut out = Vec::with_capacity(16);
        out.push(56); // ChangeGC opcode
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, gc);
        write_u32(&mut out, 1 << 2); // GC value-mask: foreground
        write_u32(&mut out, fg);
        self.stream.write_all(&out)
    }

    /// As `pick_gc_with_foreground`, but also pushes background (used by
    /// the ImageText{8,16} path which needs both colours).
    fn pick_gc_with_colors(&mut self, host_xid: u32, fg: u32, bg: u32) -> io::Result<u32> {
        let depth = self
            .host_drawable_depths
            .get(&host_xid)
            .copied()
            .unwrap_or(24);
        if depth == 24 {
            self.change_colors(fg, bg)?;
            return Ok(self.gc_id);
        }
        let gc = self.ensure_gc_for_depth(depth, host_xid)?;
        self.advance_sequence();
        let mut out = Vec::with_capacity(20);
        out.push(56);
        out.push(0);
        write_u16(&mut out, 5);
        write_u32(&mut out, gc);
        write_u32(&mut out, (1 << 2) | (1 << 3));
        write_u32(&mut out, fg);
        write_u32(&mut out, bg);
        self.stream.write_all(&out)?;
        Ok(gc)
    }

    /// Returns a host GC bound to a drawable of the given depth, creating
    /// one on demand. The default `gc_id` is depth-24; pass that drawable
    /// (or any depth-24 drawable) for that case to reuse it.
    fn ensure_gc_for_depth(&mut self, depth: u8, drawable: u32) -> io::Result<u32> {
        if depth == 24 {
            return Ok(self.gc_id);
        }
        if let Some(&gc) = self.depth_gcs.get(&depth) {
            return Ok(gc);
        }
        let gc = self.next_xid();
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(55); // CreateGC opcode
        out.push(0);
        write_u16(&mut out, 4); // length units (no values)
        write_u32(&mut out, gc);
        write_u32(&mut out, drawable);
        write_u32(&mut out, 0); // value-mask = 0
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.depth_gcs.insert(depth, gc);
        Ok(gc)
    }

    pub fn put_image(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        // GCs are bound to a drawable's root and depth at creation; using the
        // default depth-24 GC against a depth-1/8/32 pixmap would BadMatch and
        // silently discard the image data on the host.
        let gc = self.ensure_gc_for_depth(depth, host_xid)?;
        if height == 0 || data.is_empty() {
            return Ok(());
        }
        // Standard PutImage has a 16-bit length field (in 4-byte units),
        // so max body bytes = 65535 * 4 - 24 = 262_116. e16's root
        // background can be 800x600 d24 = 1.9 MB which won't fit; chunk
        // by row count to stay under the limit.
        const MAX_PAYLOAD: usize = (u16::MAX as usize - 6) * 4;
        let stride = data.len() / usize::from(height);
        if stride == 0 {
            return Ok(());
        }
        let max_rows = (MAX_PAYLOAD / stride).max(1);
        let total_rows = usize::from(height);
        let mut row = 0usize;
        while row < total_rows {
            let rows = (total_rows - row).min(max_rows);
            let chunk = &data[row * stride..(row + rows) * stride];
            let padded_data_len = padded_len(chunk.len());
            let length_units = 6 + (padded_data_len / 4) as u16;
            let chunk_dst_y = dst_y.wrapping_add(row as i16);
            let chunk_height = rows as u16;
            self.advance_sequence();
            let mut out = Vec::with_capacity(24 + padded_data_len);
            out.push(72); // PutImage opcode
            out.push(2); // ZPixmap format
            write_u16(&mut out, length_units);
            write_u32(&mut out, host_xid);
            write_u32(&mut out, gc);
            write_u16(&mut out, width);
            write_u16(&mut out, chunk_height);
            write_i16(&mut out, dst_x);
            write_i16(&mut out, chunk_dst_y);
            out.push(0); // left-pad
            out.push(depth);
            write_u16(&mut out, 0); // unused
            out.extend_from_slice(chunk);
            out.resize(24 + padded_data_len, 0);
            self.stream.write_all(&out)?;
            row += rows;
        }
        self.stream.flush()
    }

    pub fn poly_fill_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        self.draw_arcs(host_xid, 71, foreground, arcs)
    }

    pub fn poly_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        self.draw_arcs(host_xid, 68, foreground, arcs)
    }

    pub fn poly_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.draw_rectangles(host_xid, 67, foreground, rectangles)
    }

    pub fn poly_segment(
        &mut self,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        self.draw_rectangles(host_xid, 66, foreground, segments)
    }

    pub fn poly_fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.draw_rectangles(host_xid, 70, foreground, rectangles)
    }

    fn draw_rectangles(
        &mut self,
        host_xid: u32,
        opcode: u8,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        if rectangles.is_empty() {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        let length_units = 3 + u16::try_from(rectangles.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many rectangles for one X11 request",
            )
        })?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(rectangles);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let mut rectangle = Vec::with_capacity(8);
        write_i16(&mut rectangle, x);
        write_i16(&mut rectangle, y);
        write_u16(&mut rectangle, width);
        write_u16(&mut rectangle, height);
        self.poly_fill_rectangle(host_xid, foreground, &rectangle)
    }

    pub fn poly_line(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        let length_units = 3 + u16::try_from(points.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many points for one X11 request",
            )
        })?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(65);
        out.push(coordinate_mode);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(points);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn image_text8(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let gc = self.pick_gc_with_colors(host_xid, foreground, background)?;

        let text = &body[12..];
        let length_units = 4 + u16::try_from(text.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(76);
        out.push(text_len);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(&body[8..12]);
        out.extend_from_slice(text);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn image_text16(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let gc = self.pick_gc_with_colors(host_xid, foreground, background)?;

        let text = &body[12..];
        let length_units = 4 + u16::try_from(text.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(77);
        out.push(text_len);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(&body[8..12]);
        out.extend_from_slice(text);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_text8(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        let length_units = 1 + u16::try_from(body.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(74);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(&body[8..]);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_text16(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        let length_units = 1 + u16::try_from(body.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(75);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(&body[8..]);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_point(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        let length_units = 3 + u16::try_from(points.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many points for one X11 request",
            )
        })?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(64);
        out.push(coordinate_mode);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(points);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn fill_poly(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        if points.len() < 4 {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        // FillPoly opcode 69: drawable, gc, shape(1), coord-mode(1), pad(2), points...
        // length = 4 (header words) + npoints
        let length_units = 4 + u16::try_from(points.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many points for one X11 request",
            )
        })?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(69);
        out.push(0); // unused
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.push(0); // shape = Complex
        out.push(coord_mode);
        write_u16(&mut out, 0); // pad
        out.extend_from_slice(points);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    fn draw_arcs(
        &mut self,
        host_xid: u32,
        opcode: u8,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        if arcs.is_empty() {
            return Ok(());
        }
        let gc = self.pick_gc_with_foreground(host_xid, foreground)?;

        let length_units = 3 + u16::try_from(arcs.len() / 4).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "too many arcs for one X11 request")
        })?;
        self.advance_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, gc);
        out.extend_from_slice(arcs);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    fn change_foreground(&mut self, foreground: u32) -> io::Result<()> {
        if self.current_foreground == foreground {
            return Ok(());
        }

        self.advance_sequence();
        let mut out = Vec::new();
        out.push(56);
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, 1 << 2);
        write_u32(&mut out, foreground);
        self.stream.write_all(&out)?;
        self.current_foreground = foreground;
        Ok(())
    }

    fn change_colors(&mut self, foreground: u32, background: u32) -> io::Result<()> {
        if self.current_foreground == foreground && self.current_background == background {
            return Ok(());
        }

        self.advance_sequence();
        let mut out = Vec::new();
        out.push(56);
        out.push(0);
        write_u16(&mut out, 5);
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, (1 << 2) | (1 << 3));
        write_u32(&mut out, foreground);
        write_u32(&mut out, background);
        self.stream.write_all(&out)?;
        self.current_foreground = foreground;
        self.current_background = background;
        Ok(())
    }
}

#[must_use]
pub(crate) fn xkb_minor_has_reply(minor: u8) -> bool {
    // Reply-producing XKB minor requests per /usr/share/xcb/xkb.xml
    // (every minor with a `<reply>` block). xset q's request stream
    // (GetNamedIndicator = minor 15) hung indefinitely until 15 was
    // added. Today's audit confirmed that minors 14 / 16 / 18 / 20
    // (SetIndicatorMap / SetNamedIndicator / SetNames / SetGeometry)
    // are void per canonical xkb.xml — they were previously kept on
    // this list as "conservative legacy entries" but would block
    // waiting on replies the host can't generate.
    matches!(
        minor,
        0  // UseExtension
        | 4  // GetState
        | 6  // GetControls
        | 8  // GetMap
        | 10 // GetCompatMap
        | 12 // GetIndicatorState
        | 13 // GetIndicatorMap
        | 15 // GetNamedIndicator
        | 17 // GetNames
        | 19 // GetGeometry
        | 21 // PerClientFlags
        | 22 // ListComponents
        | 23 // GetKbdByName
        | 24 // GetDeviceInfo
        | 101 // SetDebuggingFlags
    )
}
/// Push a CARD8 GC value into a ChangeGC value-list, padded to 4 bytes
/// (X11 GC values are CARD32-aligned even when the semantic type is one byte).
fn push_card8_padded(out: &mut Vec<u8>, value: u8) {
    out.push(value);
    out.push(0);
    out.push(0);
    out.push(0);
}

/// Add `(x_off, y_off)` to every non-sentinel glyph command's delta in a
/// `CompositeGlyphs{8,16,32}` payload. Previously this only patched the first
/// command's delta and stopped, which mispositioned multi-run composites
/// — anything containing a 255 sentinel that switches glyphsets between runs.
///
/// `id_size` is 1 for `CompositeGlyphs8` (minor 23), 2 for the 16-bit variant
/// (minor 24), 4 for the 32-bit variant (minor 25). The glyph-id payload that
/// follows each non-sentinel header is `count * id_size` bytes, padded to a
/// 4-byte boundary; the next header begins immediately after that.
fn patch_glyph_command_offsets(items: &mut [u8], x_off: i16, y_off: i16, id_size: usize) {
    if x_off == 0 && y_off == 0 {
        return;
    }
    debug_assert!(matches!(id_size, 1 | 2 | 4));
    let mut pos = 0;
    while pos + 8 <= items.len() {
        let count = items[pos];
        if count == 255 {
            // Glyphset-switch sentinel: 8 bytes total (count, pad×3, glyphset
            // XID), no payload follows.
            pos += 8;
            continue;
        }
        let dx = i16::from_le_bytes([items[pos + 4], items[pos + 5]]).wrapping_add(x_off);
        let dy = i16::from_le_bytes([items[pos + 6], items[pos + 7]]).wrapping_add(y_off);
        items[pos + 4..pos + 6].copy_from_slice(&dx.to_le_bytes());
        items[pos + 6..pos + 8].copy_from_slice(&dy.to_le_bytes());
        let payload_bytes = usize::from(count) * id_size;
        let padded = (payload_bytes + 3) & !3;
        pos += 8 + padded;
    }
}

/// Build the wire bytes for XFIXES `ChangeCursorByName` (minor 23). Returns
/// `None` when the host XFIXES extension is unavailable.
fn build_xfixes_change_cursor_by_name(
    host_xfixes_opcode: Option<u8>,
    host_cursor_xid: u32,
    name_bytes: &[u8],
) -> Option<Vec<u8>> {
    let opcode = host_xfixes_opcode?;
    let nbytes = u16::try_from(name_bytes.len()).ok()?;
    let padded_name = padded_len(name_bytes.len());
    let length_units = u16::try_from(3 + padded_name / 4).ok()?;
    let mut out = Vec::with_capacity(12 + padded_name);
    out.push(opcode);
    out.push(yserver_protocol::x11::xfixes::CHANGE_CURSOR_BY_NAME);
    write_u16(&mut out, length_units);
    write_u32(&mut out, host_cursor_xid);
    write_u16(&mut out, nbytes);
    write_u16(&mut out, 0); // pad
    out.extend_from_slice(name_bytes);
    out.resize(12 + padded_name, 0);
    Some(out)
}

/// Build the wire bytes for SHAPE `Rectangles(op=Set, ordering=Unsorted)`
/// targeting `host_xid`. Returns `None` when the host SHAPE extension is
/// unavailable so the caller doesn't waste a stream write.
fn build_shape_rectangles(
    host_shape_opcode: Option<u8>,
    host_xid: u32,
    kind: u8,
    rects: &[yserver_protocol::x11::xfixes::RegionRect],
) -> Option<Vec<u8>> {
    let opcode = host_shape_opcode?;
    let length_units = u16::try_from(4 + rects.len() * 2).ok()?;
    let mut out = Vec::with_capacity(16 + rects.len() * 8);
    out.push(opcode);
    out.push(yserver_protocol::x11::shape::RECTANGLES);
    write_u16(&mut out, length_units);
    out.push(yserver_protocol::x11::shape::OP_SET);
    out.push(kind);
    out.push(0); // ordering = Unsorted
    out.push(0); // pad
    write_u32(&mut out, host_xid);
    write_i16(&mut out, 0); // x_off (top-level coords already match)
    write_i16(&mut out, 0); // y_off
    for rect in rects {
        write_i16(&mut out, rect.x);
        write_i16(&mut out, rect.y);
        write_u16(&mut out, rect.width);
        write_u16(&mut out, rect.height);
    }
    Some(out)
}

fn write_open_font(stream: &mut HostStream, font_id: u32, name: &[u8]) -> io::Result<()> {
    open_font(stream, font_id, name)
}

fn write_query_font(stream: &mut HostStream, font_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(47);
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, font_id);
    stream.write_all(&out)
}

fn write_list_fonts(
    stream: &mut HostStream,
    opcode: u8,
    max_names: u16,
    pattern: &[u8],
) -> io::Result<()> {
    let padded = padded_len(pattern.len());
    let length_units = 2 + u16::try_from(padded / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font pattern is too long"))?;

    let mut out = Vec::new();
    out.push(opcode);
    out.push(0);
    write_u16(&mut out, length_units);
    write_u16(&mut out, max_names);
    write_u16(
        &mut out,
        u16::try_from(pattern.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font pattern is too long"))?,
    );
    out.extend_from_slice(pattern);
    out.resize(8 + padded, 0);
    stream.write_all(&out)
}
#[cfg(test)]
mod tests {
    use super::{build_shape_rectangles, xkb_minor_has_reply};
    use yserver_protocol::x11::xfixes::RegionRect;

    #[test]
    fn xkb_reply_minor_audit_includes_known_blocking_requests() {
        // Reply-required minors per /usr/share/xcb/xkb.xml (every
        // minor that has a `<reply>` block). Adding 15
        // (`GetNamedIndicator`), 19 (`GetGeometry`), 23 (`GetKbdByName`)
        // and 13 (`GetIndicatorMap`) was load-bearing for `xset q`,
        // GTK keyboard layout queries, and libxkbcommon probes.
        for minor in [0, 4, 6, 8, 10, 12, 13, 15, 17, 19, 21, 22, 23, 24, 101] {
            assert!(
                xkb_minor_has_reply(minor),
                "minor {minor} must wait for reply"
            );
        }
    }

    #[test]
    fn xkb_void_minor_audit_keeps_select_events_fire_and_forget() {
        // Per canonical xkb.xml these have no <reply> block.
        assert!(!xkb_minor_has_reply(1)); // SelectEvents
        assert!(!xkb_minor_has_reply(2)); // (no-op — Bell is core minor 3)
        assert!(!xkb_minor_has_reply(3)); // Bell
        assert!(!xkb_minor_has_reply(5)); // LatchLockState
        assert!(!xkb_minor_has_reply(14)); // SetIndicatorMap
        assert!(!xkb_minor_has_reply(16)); // SetNamedIndicator
        assert!(!xkb_minor_has_reply(18)); // SetNames
        assert!(!xkb_minor_has_reply(20)); // SetGeometry
    }

    #[test]
    fn shape_rectangles_no_host_opcode_returns_none() {
        // No host SHAPE opcode → caller should send nothing.
        assert!(build_shape_rectangles(None, 0xdead_beef, 0, &[]).is_none());
    }

    #[test]
    fn change_cursor_by_name_no_host_opcode_returns_none() {
        // No host XFIXES opcode → caller should send nothing and not crash.
        assert!(super::build_xfixes_change_cursor_by_name(None, 0x10, b"left_ptr").is_none());
    }

    #[test]
    fn change_cursor_by_name_wire_shape_unpadded_name() {
        // 8-byte name "left_ptr" → no padding needed.
        let bytes =
            super::build_xfixes_change_cursor_by_name(Some(140), 0xdead_beef, b"left_ptr").unwrap();
        // header(4) + cursor(4) + nbytes(2)+pad(2) + name(8) = 20 bytes = 5 units
        assert_eq!(bytes.len(), 20);
        assert_eq!(bytes[0], 140, "major = XFIXES");
        assert_eq!(bytes[1], 27, "minor = ChangeCursorByName per xfixes.xml");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 5);
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            0xdead_beef,
        );
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 8);
        assert_eq!(&bytes[12..20], b"left_ptr");
    }

    #[test]
    fn change_cursor_by_name_wire_shape_pads_name_to_4() {
        // 5-byte name → 3 padding bytes appended.
        let bytes = super::build_xfixes_change_cursor_by_name(Some(140), 0x1, b"hand1").unwrap();
        // 12 (header) + padded_len(5) = 12 + 8 = 20 bytes = 5 units
        assert_eq!(bytes.len(), 20);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 5);
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 5);
        assert_eq!(&bytes[12..17], b"hand1");
        assert_eq!(&bytes[17..20], &[0, 0, 0]);
    }

    fn glyphcmd(count: u8, dx: i16, dy: i16) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0] = count;
        buf[4..6].copy_from_slice(&dx.to_le_bytes());
        buf[6..8].copy_from_slice(&dy.to_le_bytes());
        buf
    }

    fn read_dx(items: &[u8], pos: usize) -> i16 {
        i16::from_le_bytes([items[pos + 4], items[pos + 5]])
    }

    fn read_dy(items: &[u8], pos: usize) -> i16 {
        i16::from_le_bytes([items[pos + 6], items[pos + 7]])
    }

    #[test]
    fn glyph_offset_patches_every_non_255_command() {
        // Two real glyph runs (count=1, count=2) separated by a 255 sentinel
        // marking a glyphset switch. Every non-sentinel command's delta must
        // be shifted by (x_off, y_off) so multi-run composites line up at the
        // host's destination origin.
        let mut items = Vec::new();
        items.extend_from_slice(&glyphcmd(1, 10, 20)); // run 1
        items.extend_from_slice(&glyphcmd(255, 0, 0)); // glyphset switch
        items.extend_from_slice(&[0u8; 4]); // 8-byte alignment for the 255 cmd
        items.extend_from_slice(&glyphcmd(2, 30, 40)); // run 2

        super::patch_glyph_command_offsets(&mut items, 5, -3, 1);

        assert_eq!(read_dx(&items, 0), 15);
        assert_eq!(read_dy(&items, 0), 17);
        // 255 sentinel cmd untouched (its delta bytes are still 0).
        assert_eq!(read_dx(&items, 8), 0);
        assert_eq!(read_dy(&items, 8), 0);
        // Second run patched too.
        assert_eq!(read_dx(&items, 20), 35);
        assert_eq!(read_dy(&items, 20), 37);
    }

    #[test]
    fn glyph_offset_zero_offset_is_identity() {
        let mut items = Vec::new();
        items.extend_from_slice(&glyphcmd(1, 10, 20));
        items.extend_from_slice(&glyphcmd(2, 30, 40));
        let original = items.clone();
        super::patch_glyph_command_offsets(&mut items, 0, 0, 1);
        assert_eq!(items, original);
    }

    #[test]
    fn glyph_offset_handles_16bit_glyph_id_stride() {
        // Two CompositeGlyphs16 runs back to back: header(8) + count*2 padded
        // to 4. count=3 → 6 bytes payload → padded 8 bytes → next header at
        // pos+16.
        let mut items = Vec::new();
        items.extend_from_slice(&glyphcmd(3, 10, 20));
        items.extend_from_slice(&[0xaa, 0xaa, 0xbb, 0xbb, 0xcc, 0xcc, 0, 0]); // 3 ids + pad
        items.extend_from_slice(&glyphcmd(2, 30, 40));
        items.extend_from_slice(&[0xdd, 0xdd, 0xee, 0xee]); // 2 ids, no pad needed

        super::patch_glyph_command_offsets(&mut items, 1, 2, 2);

        assert_eq!(read_dx(&items, 0), 11);
        assert_eq!(read_dy(&items, 0), 22);
        assert_eq!(read_dx(&items, 16), 31);
        assert_eq!(read_dy(&items, 16), 42);
    }

    #[test]
    fn shape_rectangles_empty_list_clears_shape() {
        // Empty rect list with op=Set is the canonical "no shape" — header only,
        // no rectangle bodies.
        let bytes = build_shape_rectangles(Some(141), 0xdead_beef, 0, &[]).unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes[0], 141, "major opcode");
        assert_eq!(bytes[1], 1, "minor = RECTANGLES");
        assert_eq!(
            u16::from_le_bytes([bytes[2], bytes[3]]),
            4,
            "length = 4 units"
        );
        assert_eq!(bytes[4], 0, "op = Set");
        assert_eq!(bytes[5], 0, "kind = Bounding");
        assert_eq!(bytes[6], 0, "ordering = Unsorted");
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            0xdead_beef,
            "dest = host xid",
        );
        // x_off, y_off are zero for top-level forwarding.
        assert_eq!(i16::from_le_bytes([bytes[12], bytes[13]]), 0);
        assert_eq!(i16::from_le_bytes([bytes[14], bytes[15]]), 0);
    }

    #[test]
    fn shape_rectangles_clip_kind_mapping() {
        // KIND_CLIP is 1; ensure it makes it onto the wire as-is.
        let bytes = build_shape_rectangles(Some(141), 0x1, 1, &[]).unwrap();
        assert_eq!(bytes[5], 1, "kind = Clip");
    }

    #[test]
    fn shape_rectangles_multi_rect_payload_unchanged() {
        // Top-level shape coords are already in the host subwindow's local
        // coords (subwindow position itself is the offset). Forwarder must
        // not translate per-rect — assert the rectangle bytes are forwarded
        // verbatim.
        let rects = [
            RegionRect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            RegionRect {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            },
        ];
        let bytes = build_shape_rectangles(Some(141), 0xabad_cafe, 0, &rects).unwrap();
        // header(16) + 2 rects * 8 bytes = 32 bytes = 8 units
        assert_eq!(bytes.len(), 32);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 8);

        assert_eq!(i16::from_le_bytes([bytes[16], bytes[17]]), 1);
        assert_eq!(i16::from_le_bytes([bytes[18], bytes[19]]), 2);
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 3);
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 4);

        assert_eq!(i16::from_le_bytes([bytes[24], bytes[25]]), 10);
        assert_eq!(i16::from_le_bytes([bytes[26], bytes[27]]), 20);
        assert_eq!(u16::from_le_bytes([bytes[28], bytes[29]]), 30);
        assert_eq!(u16::from_le_bytes([bytes[30], bytes[31]]), 40);
    }
}
