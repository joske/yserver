//! `impl Backend for HostX11Backend` — every method delegates to the
//! existing `HostX11Backend::method` directly. The body is intentionally
//! mechanical so a future trait surface change (Phase 6.3+) can land
//! by editing one file.

use std::io;

use yserver_protocol::x11::{ClipRectangles, FontMetrics, ResourceId, xfixes};

use crate::backend::{
    AnyHandle, Backend, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
    PictureHandle, PixmapHandle, WindowHandle,
};

use super::{
    HostSubwindowConfig, HostSubwindowVisual, HostX11Backend, HostXidMap, OriginContext,
    PointerPosition,
};

impl Backend for HostX11Backend {
    fn window_id(&self) -> u32 {
        HostX11Backend::window_id(self)
    }

    fn root_visual_xid(&self) -> u32 {
        HostX11Backend::root_visual_xid(self)
    }

    fn argb_visual_xid(&self) -> Option<u32> {
        HostX11Backend::argb_visual_xid(self)
    }

    fn argb_colormap_xid(&self) -> Option<u32> {
        HostX11Backend::argb_colormap_xid(self)
    }

    fn render_opcode(&self) -> Option<u8> {
        HostX11Backend::render_opcode(self)
    }

    fn xkb_opcode(&self) -> Option<u8> {
        HostX11Backend::xkb_opcode(self)
    }

    fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        HostX11Backend::xkb_info(self)
    }

    fn composite_opcode(&self) -> Option<u8> {
        HostX11Backend::composite_opcode(self)
    }

    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        HostX11Backend::render_format_for_ynest_id(self, ynest_fmt)
    }

    fn ping(&mut self, origin: Option<OriginContext>) -> io::Result<()> {
        self.with_active_origin(origin, HostX11Backend::ping)
    }

    fn on_host_input(
        &mut self,
        _state: &mut crate::server::ServerState,
        ev: crate::core_loop::HostInputEvent,
    ) {
        // Real host pointer/key events arrive via `drain_host_socket` and
        // go straight onto `pending_events`; this method is normally a
        // no-op. XTEST `FakeInput` reuses the same enqueue path so
        // injected events flow through the existing fanout pipeline.
        use crate::{
            core_loop::HostInputEvent,
            host_x11::{HostEvent, HostKeyEvent, HostPointerEvent, PointerEventKind},
        };

        let container = self.window_id();
        match ev {
            HostInputEvent::Key(raw) => {
                self.push_pending_host_event(HostEvent::Key(HostKeyEvent {
                    pressed: raw.pressed,
                    keycode: raw.keycode,
                    time: raw.time,
                    root_x: raw.root_x,
                    root_y: raw.root_y,
                    event_x: raw.event_x,
                    event_y: raw.event_y,
                    state: raw.state,
                }));
            }
            HostInputEvent::PointerMotion { x, y, time, .. } => {
                self.push_pending_host_event(HostEvent::Pointer(HostPointerEvent {
                    kind: PointerEventKind::MotionNotify,
                    host_xid: container,
                    detail: 0,
                    time,
                    root_x: x as i16,
                    root_y: y as i16,
                    event_x: x as i16,
                    event_y: y as i16,
                    state: 0,
                    crossing_mode: 0,
                    child: 0,
                }));
            }
            HostInputEvent::PointerButton {
                button,
                pressed,
                time,
            } => {
                // `button` is a Linux input code (BTN_LEFT = 0x110, …).
                // Translate to X11 button numbers — same mapping as
                // `KmsBackend::process_pointer_button`.
                let detail = match button {
                    0x110 => 1, // BTN_LEFT
                    0x111 => 3, // BTN_RIGHT
                    0x112 => 2, // BTN_MIDDLE
                    0x113 => 8, // BTN_SIDE
                    0x114 => 9, // BTN_EXTRA
                    0x180 => 4, // SYNTH_SCROLL_UP
                    0x181 => 5, // SYNTH_SCROLL_DOWN
                    0x182 => 6, // SYNTH_SCROLL_LEFT
                    0x183 => 7, // SYNTH_SCROLL_RIGHT
                    _ => {
                        log::debug!(
                            "ynest: dropping HostInputEvent::PointerButton for unknown linux code 0x{button:x}"
                        );
                        return;
                    }
                };
                let kind = if pressed {
                    PointerEventKind::ButtonPress
                } else {
                    PointerEventKind::ButtonRelease
                };
                self.push_pending_host_event(HostEvent::Pointer(HostPointerEvent {
                    kind,
                    host_xid: container,
                    detail,
                    time,
                    root_x: 0,
                    root_y: 0,
                    event_x: 0,
                    event_y: 0,
                    state: 0,
                    crossing_mode: 0,
                    child: 0,
                }));
            }
            // Device add/remove are plumbing-only in the host-X11 backend;
            // the nested backend has no XI2 device registry of its own.
            HostInputEvent::DeviceAdded(_) | HostInputEvent::DeviceRemoved { .. } => {}
        }
    }

    /// Host-X11 backend never page-flips.
    fn on_page_flip_ready(&mut self, _state: &mut crate::server::ServerState) {}

    /// F2: host fd registered with the core's poller as
    /// `HOST_X11_TOKEN`. Readiness drives `drain_host_socket`.
    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, crate::backend::BackendFdKind)> {
        vec![(self.host_fd(), crate::backend::BackendFdKind::HostX11)]
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn create_subwindow(
        &mut self,
        origin: Option<OriginContext>,
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
        self.with_active_origin(origin, |this| {
            HostX11Backend::create_subwindow(
                this,
                host_parent,
                x,
                y,
                width,
                height,
                border_width,
                visual,
                background_pixel,
                background_pixmap,
            )
        })
    }

    fn destroy_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::destroy_subwindow(this, host_xid)
        })
    }

    fn map_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::map_subwindow(this, host_xid))
    }

    fn unmap_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::unmap_subwindow(this, host_xid)
        })
    }

    fn configure_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::configure_subwindow(this, host_xid, config)
        })
    }

    fn reparent_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::reparent_subwindow(this, host_xid, host_parent, x, y)
        })
    }

    fn change_subwindow_attributes(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::change_subwindow_attributes(this, host_xid, value_mask, values)
        })
    }

    fn update_host_event_mask(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            // Phase 6.3 Step 4: persist the new combined mask to the
            // wire too, otherwise the host child never learns about
            // the change. Pre-Step-4 the per-pump pump-handle
            // `register_*` issued the ChangeWindowAttributes write on
            // a separate socket; with one merged connection the
            // registry update *is* the wire write.
            let combined = HostX11Backend::update_host_event_mask(this, host_xid, mask, enabled);
            HostX11Backend::write_event_mask(this, host_xid, combined)
        })
    }

    fn register_top_level(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::register_top_level(this, nested_id, host_xid)
        })
    }

    fn register_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::register_subwindow(this, nested_id, host_xid)
        })
    }

    fn unregister_host_window(&mut self, host_xid: u32) {
        HostX11Backend::unregister_host_window(self, host_xid);
    }

    fn xid_map(&self) -> &HostXidMap {
        HostX11Backend::xid_map(self)
    }

    fn drain_host_socket(&mut self) -> io::Result<crate::backend::HostSocketStatus> {
        HostX11Backend::drain_host_socket(self)
    }

    fn pop_pending_host_event(&mut self) -> Option<crate::host_x11::HostEvent> {
        HostX11Backend::pop_pending_host_event(self)
    }

    fn host_socket_eof(&self) -> bool {
        HostX11Backend::host_socket_eof(self)
    }

    fn name_window_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::name_window_pixmap(this, host_window)
        })
    }

    fn create_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::create_pixmap(this, depth, width, height)
        })
    }

    fn free_pixmap(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::free_pixmap(this, host_xid))
    }

    fn open_font(
        &mut self,
        origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)> {
        self.with_active_origin(origin, |this| HostX11Backend::open_font(this, name))
    }

    fn close_font(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::close_font(this, host_xid))
    }

    fn create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::create_cursor(
                this,
                source_pixmap,
                mask_pixmap,
                fore,
                back,
                hot_x,
                hot_y,
            )
        })
    }

    fn free_cursor(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::free_cursor(this, host_xid))
    }

    fn create_glyph_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_font: FontHandle,
        mask_font: Option<FontHandle>,
        source_char: u16,
        mask_char: u16,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
    ) -> io::Result<CursorHandle> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::create_glyph_cursor(
                this,
                source_font,
                mask_font,
                source_char,
                mask_char,
                fore,
                back,
            )
        })
    }

    fn define_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::define_cursor(this, host_window_xid, cursor_host_xid)
        })
    }

    fn set_container_background_pixel(
        &mut self,
        origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::set_container_background_pixel(this, pixel)
        })
    }

    fn set_container_background_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::set_container_background_pixmap(this, host_pixmap_xid)
        })
    }

    fn clear_clip_rectangles(&mut self, origin: Option<OriginContext>) -> io::Result<()> {
        self.with_active_origin(origin, HostX11Backend::clear_clip_rectangles)
    }

    fn set_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::set_clip_rectangles(this, clip)
        })
    }

    fn set_clip_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::set_clip_pixmap(this, host_pixmap, clip_x_origin, clip_y_origin)
        })
    }

    fn set_gc_fill_solid(&mut self, origin: Option<OriginContext>) -> io::Result<()> {
        self.with_active_origin(origin, HostX11Backend::set_gc_fill_solid)
    }

    fn set_gc_fill_tiled(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::set_gc_fill_tiled(this, host_pixmap, tile_x_origin, tile_y_origin)
        })
    }

    fn apply_clip_state(
        &mut self,
        origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::apply_clip_state(this, clip))
    }

    fn apply_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::apply_fill_state(this, fill))
    }

    fn apply_draw_state(
        &mut self,
        origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| HostX11Backend::apply_draw_state(this, state))
    }

    fn copy_area(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::copy_area(
                this,
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
            )
        })
    }

    fn copy_plane(
        &mut self,
        origin: Option<OriginContext>,
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
        self.with_active_origin(origin, |this| {
            HostX11Backend::copy_plane(
                this,
                src_host_xid,
                dst_host_xid,
                src_x,
                src_y,
                dst_x,
                dst_y,
                width,
                height,
                plane,
            )
        })
    }

    fn put_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::put_image(this, host_xid, depth, width, height, dst_x, dst_y, data)
        })
    }

    fn get_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::get_image(this, host_xid, format, x, y, width, height, plane_mask)
        })
    }

    fn clear_area(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        _background_pixel: u32,
        _background_pixmap_host_xid: Option<u32>,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        _tile_origin: (i32, i32),
    ) -> io::Result<()> {
        // ynest forwards ClearArea to the host, which resolves the
        // window's background (incl. ParentRelative alignment) itself.
        self.with_active_origin(origin, |this| {
            HostX11Backend::clear_area(this, host_xid, x, y, width, height, false)
        })
    }

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_line(this, host_xid, foreground, coordinate_mode, points)
        })
    }

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_segment(this, host_xid, foreground, segments)
        })
    }

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_rectangle(this, host_xid, foreground, rectangles)
        })
    }

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_arc(this, host_xid, foreground, arcs)
        })
    }

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_point(this, host_xid, foreground, coordinate_mode, points)
        })
    }

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_fill_rectangle(this, host_xid, foreground, rectangles)
        })
    }

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_fill_arc(this, host_xid, foreground, arcs)
        })
    }

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::fill_poly(this, host_xid, foreground, coord_mode, points)
        })
    }

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::fill_rectangle(this, host_xid, foreground, x, y, width, height)
        })
    }

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_text8(this, host_xid, foreground, body)
        })
    }

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::poly_text16(this, host_xid, foreground, body)
        })
    }

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::image_text8(this, host_xid, foreground, background, text_len, body)
        })
    }

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::image_text16(this, host_xid, foreground, background, text_len, body)
        })
    }

    fn render_create_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_create_picture(
                this,
                host_drawable,
                ynest_format,
                value_mask,
                values,
            )
        })
    }

    fn render_change_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_change_picture(this, host_pic, body)
        })
    }

    fn render_free_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_free_picture(this, host_pic)
        })
    }

    fn render_create_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_create_glyphset(this, ynest_format)
        })
    }

    fn render_free_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_free_glyphset(this, host_gs)
        })
    }

    fn render_add_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_add_glyphs(this, host_gs, body_tail)
        })
    }

    fn render_free_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_free_glyphs(this, host_gs, glyph_ids)
        })
    }

    fn render_composite(
        &mut self,
        origin: Option<OriginContext>,
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
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_composite(
                this, op, host_src, host_mask, host_dst, src_x, src_y, mask_x, mask_y, dst_x,
                dst_y, width, height,
            )
        })
    }

    fn render_composite_glyphs(
        &mut self,
        origin: Option<OriginContext>,
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
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_composite_glyphs(
                this, minor, op, host_src, host_dst, mask_fmt, host_gs, src_x, src_y, items, x_off,
                y_off,
            )
        })
    }

    fn render_fill_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_fill_rectangles(this, host_dst, op, color, rects, x_off, y_off)
        })
    }

    fn render_trapezoids(
        &mut self,
        origin: Option<OriginContext>,
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
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_trapezoids(
                this,
                op,
                host_src,
                host_dst,
                host_mask_format,
                src_x,
                src_y,
                traps,
                x_off,
                y_off,
            )
        })
    }

    fn render_triangles_op(
        &mut self,
        origin: Option<OriginContext>,
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
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_triangles_op(
                this,
                minor,
                op,
                host_src,
                host_dst,
                host_mask_format,
                src_x,
                src_y,
                primitives,
                x_off,
                y_off,
            )
        })
    }

    fn render_create_solid_fill(
        &mut self,
        origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_create_solid_fill(this, color)
        })
    }

    fn render_create_linear_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_create_linear_gradient(this, body)
        })
    }

    fn render_create_radial_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_create_radial_gradient(this, body)
        })
    }

    fn render_create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_create_cursor(this, host_src_pic, x, y)
        })
    }

    fn render_set_picture_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_set_picture_clip_rectangles(this, host_pic, body)
        })
    }

    fn render_set_picture_filter(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_set_picture_filter(this, host_pic, body)
        })
    }

    fn render_set_picture_transform(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::render_set_picture_transform(this, host_pic, body)
        })
    }

    fn render_query_version(&mut self, origin: Option<OriginContext>) -> io::Result<(u32, u32)> {
        self.with_active_origin(origin, HostX11Backend::render_query_version)
    }

    fn xkb_proxy(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Option<Vec<u8>>> {
        // Host-proxied path: the real X server supplies VirtualModNames,
        // so the local interner is unused here.
        self.with_active_origin(origin, |this| HostX11Backend::xkb_proxy(this, minor, body))
    }

    fn xfixes_change_cursor_by_name(
        &mut self,
        origin: Option<OriginContext>,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::xfixes_change_cursor_by_name(this, host_cursor_xid, name_bytes)
        })
    }

    fn set_shape_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::set_shape_rectangles(this, host_xid, kind, rects)
        })
    }

    fn warp_pointer(
        &mut self,
        origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::warp_pointer(this, dst_host_xid, dst_x, dst_y)
        })
    }

    fn query_pointer(&mut self, origin: Option<OriginContext>) -> io::Result<PointerPosition> {
        self.with_active_origin(origin, HostX11Backend::query_pointer)
    }

    fn list_fonts_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::list_fonts_proxy(this, max_names, pattern)
        })
    }

    fn list_fonts_with_info_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
        // Host-X11 proxy forwards the host server's replies verbatim
        // (the host already attaches FONT properties); no atoms to
        // intern on this side.
        _intern_atom: &mut dyn FnMut(&str) -> u32,
    ) -> io::Result<Vec<Vec<u8>>> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::list_fonts_with_info_proxy(this, max_names, pattern)
        })
    }

    fn get_atom_name(
        &mut self,
        origin: Option<OriginContext>,
        atom: u32,
    ) -> io::Result<Option<String>> {
        self.with_active_origin(origin, |this| HostX11Backend::get_atom_name(this, atom))
    }

    fn get_keyboard_mapping(
        &mut self,
        origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        self.with_active_origin(origin, |this| {
            HostX11Backend::get_keyboard_mapping(this, first_keycode, count)
        })
    }

    fn get_modifier_mapping(&mut self, origin: Option<OriginContext>) -> io::Result<(u8, Vec<u8>)> {
        self.with_active_origin(origin, HostX11Backend::get_modifier_mapping)
    }
}
