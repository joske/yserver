use super::*;

impl KmsBackend {
    // ── Stage 3a: Core-text helpers ─────────────────────────────

    /// FreeType rasterise + atlas dispatch for one text run.
    /// Used by `image_text8/16` and `poly_text8/16`. Per Stage 3
    /// plan §"Cross-cutting" §4: Core ops consult GC clip only —
    /// here we don't push the GC clip into the RENDER pipeline
    /// because the text pipeline doesn't honour scissor (lives in
    /// Stage 3e). v1's path has the same limitation; promoted to
    /// a Risk item rather than blocking 3a.
    /// Rasterize core-protocol text MONOCHROME and submit it through
    /// the rop-correct span path (`fill_solid_rects`): X11 core text
    /// is binary fg coverage — PolyText honors the GC function /
    /// plane-mask, ImageText forces GXcopy (callers swap
    /// `core.current_function`). Spans are window-local;
    /// `fill_solid_rects` applies `target.offset()` — the SINGLE
    /// translation point (no per-glyph pre-shift here).
    pub(in crate::kms::render::backend) fn render_text_chars(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        let Some(font_xid) = self.core.current_font else {
            return Ok(());
        };
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        // Rasterise glyphs in a tight FreeType-borrow scope so the
        // subsequent &mut self fill call doesn't conflict.
        let mut spans: Vec<Rectangle16> = Vec::new();
        let mut cursor_x = x;
        {
            let Some(fs) = self.core.fonts.get(&font_xid) else {
                return Ok(());
            };
            let default_ch = char::from_u32(u32::from(fs.metrics.default_char));
            for &ch in text {
                // Nonexistent chars draw the font's default_char; if
                // that doesn't exist either, nothing is drawn (X11).
                let (ch, ci) = match fs.char_info_cache.get(&ch) {
                    Some(ci) => (ch, ci),
                    None => {
                        match default_ch.and_then(|d| fs.char_info_cache.get(&d).map(|ci| (d, ci)))
                        {
                            Some(pair) => pair,
                            None => continue,
                        }
                    }
                };
                let glyph_spans = {
                    let cache = fs.glyph_span_cache.borrow();
                    cache.get(&ch).cloned()
                }
                .unwrap_or_else(|| {
                    let mut face = fs.face.borrow_mut();
                    let cached = rasterize_glyph_mono_spans(&mut face.0, ch);
                    fs.glyph_span_cache.borrow_mut().insert(ch, cached.clone());
                    cached
                });
                translate_glyph_spans(&glyph_spans, cursor_x, y, &mut spans);
                cursor_x = cursor_x.saturating_add(ci.character_width as i32);
            }
        }
        if spans.is_empty() {
            return Ok(());
        }
        self.paint_solid_spans(origin, host_xid, target, foreground, spans);
        Ok(())
    }

    /// Solid spans in `host_xid`'s coordinates, clipped as a stroke is:
    /// the GC clip, the subwindow mode (inferiors included or cut out)
    /// and the window's clip in a backing it shares. Core text and
    /// points: Xorg draws them through the GC's composite clip like
    /// every other op (`fb/fbglyph.c`, `fb/fbpoint.c`).
    fn paint_solid_spans(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        target: PaintTarget,
        color: u32,
        spans: Vec<Rectangle16>,
    ) {
        let background = self.core.current_background;
        self.emit_stroke_output(
            origin,
            host_xid,
            target,
            color,
            background,
            crate::kms::render::stroke::StrokeOutput {
                fg_rects: spans,
                bg_rects: Vec::new(),
            },
        );
    }

    /// Legacy GPU-atlas text path — unreachable from the core
    /// protocol since text moved to the span path; kept for the
    /// engine plumbing until the atlas gets a new consumer or is
    /// removed in a follow-up.
    #[allow(dead_code)]
    fn render_text_chars_atlas(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i32,
        y: i32,
        text: &[char],
    ) -> io::Result<()> {
        use crate::kms::render::engine::PreparedGlyph;

        let Some(font_xid) = self.core.current_font else {
            return Ok(());
        };
        // Stage 4a — resolve through redirect routing. Glyph
        // `dst_x` / `dst_y` per `PreparedGlyph` get the
        // window→backing translation applied below.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        let (paint_dx, paint_dy) = target.offset();
        // Rasterise glyphs in a tight FreeType-borrow scope so the
        // subsequent &mut self engine call doesn't conflict.
        let mut rendered: Vec<PreparedGlyph> = Vec::with_capacity(text.len());
        let mut cursor_x = x;
        {
            let Some(fs) = self.core.fonts.get(&font_xid) else {
                return Ok(());
            };
            let face = fs.face.borrow();
            let char_cache = &fs.char_info_cache;
            for &ch in text {
                let Some(ci) = char_cache.get(&ch) else {
                    cursor_x = cursor_x.saturating_add(6);
                    continue;
                };
                let _ = face
                    .0
                    .load_char(ch as usize, freetype::face::LoadFlag::RENDER);
                let glyph = face.0.glyph();
                let bitmap = glyph.bitmap();
                if bitmap.width() > 0 && bitmap.rows() > 0 {
                    let w = bitmap.width() as usize;
                    let h = bitmap.rows() as usize;
                    let stride = bitmap.pitch();
                    let buf = bitmap.buffer();
                    let mut pixels = vec![0u8; w * h];
                    for row in 0..h {
                        let src = if stride >= 0 {
                            row * stride as usize
                        } else {
                            (h - 1 - row) * (stride as isize).unsigned_abs()
                        };
                        pixels[row * w..row * w + w].copy_from_slice(&buf[src..src + w]);
                    }
                    rendered.push(PreparedGlyph {
                        dst_x: cursor_x + glyph.bitmap_left() + paint_dx,
                        dst_y: y - glyph.bitmap_top() + paint_dy,
                        w,
                        h,
                        pixels,
                        codepoint: ch as u32,
                    });
                }
                cursor_x = cursor_x.saturating_add(ci.character_width as i32);
            }
        }
        if rendered.is_empty() {
            return Ok(());
        }
        let foreground_rgba = [
            ((foreground >> 16) & 0xFF) as f32 / 255.0,
            ((foreground >> 8) & 0xFF) as f32 / 255.0,
            (foreground & 0xFF) as f32 / 255.0,
            1.0,
        ];
        match self.engine.image_text(
            &mut self.store,
            &mut self.platform,
            target.dst(),
            font_xid,
            foreground_rgba,
            &rendered,
        ) {
            Ok(stats) => {
                for _ in 0..stats.atlas_interns {
                    self.telemetry.record_atlas_intern();
                }
                for _ in 0..stats.glyph_uploads {
                    self.telemetry.record_glyph_upload();
                }
                for _ in 0..stats.glyphs_dropped {
                    self.telemetry.record_glyph_dropped_atlas_full();
                }
                if stats.glyph_uploads > 0 {
                    // The glyph upload CB is a separate submit
                    // from the text-paint CB. Emit one
                    // GlyphUpload event per upload submit so
                    // analysis can correlate upload bursts with
                    // text bursts.
                    let target_kind = self.submit_target_kind(target.backing_id());
                    for _ in 0..stats.glyph_uploads {
                        self.telemetry.record_submit_event(SubmitEvent {
                            frame_id: 0,
                            kind: SubmitKind::GlyphUpload,
                            target_kind,
                            target_id: target.backing_id().as_u64(),
                            batch_size: 1,
                            op: SubmitOp::None,
                            src_class: SrcClass::None,
                            mask_class: SrcClass::None,
                            pipeline_id: None,
                            flags: SubmitFlags {
                                readback: false,
                                alias: false,
                                zero_draws: false,
                                upload: true,
                            },
                        });
                    }
                }
                if stats.atlas_interns > 0 || !rendered.is_empty() {
                    self.telemetry.record_paint_submit();
                    let batch_size = u32::try_from(rendered.len()).unwrap_or(u32::MAX);
                    self.trace_simple(SubmitKind::ImageText, target.backing_id(), batch_size);
                }
            }
            Err(e) => {
                log::warn!(
                    "render image_text: engine error xid={host_xid:#x}: {e:?} — dropping run"
                );
            }
        }
        Ok(())
    }

    /// `image_text8/16` background-fill helper. Lowers the
    /// per-call rect to an `engine.fill_rect` op via the same
    /// path `fill_rectangle` (Stage 2c) uses, so the bg drawn
    /// here lives on the same storage as the glyph quads.
    /// Unused since ImageText moved to the rop span path (the bg
    /// box goes through `fill_solid_rects` for clip + plane-mask);
    /// kept with the atlas path until that's removed.
    #[allow(dead_code)]
    fn fill_text_background(
        &mut self,
        host_xid: u32,
        background: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> io::Result<()> {
        if w <= 0 || h <= 0 {
            return Ok(());
        }
        // Stage 4a — resolve through redirect; rect origin is
        // shifted by the descendant→ancestor-backing offset.
        let Some(target) = self.resolve_paint_target(host_xid) else {
            return Ok(());
        };
        // L1 server-α invariant per `fill_solid_rects` (see comment
        // there): force α=1 on depth!=32 dsts so the scene
        // compositor's alpha_passthrough path doesn't blend the
        // text bg out.
        let depth = self
            .store
            .get(target.backing_id())
            .map(|d| d.depth)
            .unwrap_or(24);
        let format = self
            .store
            .get(target.backing_id())
            .map(|d| d.storage.format)
            .unwrap_or_else(|| PlatformBackend::format_for_depth(depth));
        let color = decode_x11_pixel_for_storage(background, depth, format);
        let rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D {
                x: x + target.offset().0,
                y: y + target.offset().1,
            },
            extent: ash::vk::Extent2D {
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
            },
        };
        if let Err(e) = self.engine.fill_rect(
            &mut self.store,
            &mut self.platform,
            target.dst(),
            rect,
            color,
        ) {
            log::warn!("render image_text bg fill: engine.fill_rect xid={host_xid:#x}: {e:?}");
        } else {
            self.telemetry.record_paint_submit();
            self.trace_simple(SubmitKind::FillOne, target.backing_id(), 1);
        }
        Ok(())
    }

    /// ImageText8/16 core: per X11 §8 the GC function and fill-style
    /// are IGNORED (effective GXcopy, solid); plane-mask and clip
    /// still apply. Port of Xorg miImageGlyphBlt (mi/miglblt.c:83):
    /// ONE background box from the run's overall extents —
    /// (x, y−font_ascent, overall_width, ascent+descent) — then the
    /// fg glyphs, both through the rop span path with the function
    /// temporarily forced to Copy.
    pub(in crate::kms::render::backend) fn image_text_common(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        x: i32,
        y: i32,
        chars: &[char],
    ) -> io::Result<()> {
        use yserver_core::backend::GcFunction;
        let saved_function = self.core.current_function;
        self.core.current_function = GcFunction::Copy;
        if let Some(font_state) = self.core.current_font.and_then(|f| self.core.fonts.get(&f)) {
            let total_width = text_advance(font_state, chars);
            let ascent = i32::from(font_state.metrics.font_ascent);
            let descent = i32::from(font_state.metrics.font_descent);
            let bg_w = total_width.clamp(0, i32::from(i16::MAX));
            let bg_h = (ascent + descent).clamp(0, i32::from(i16::MAX));
            if bg_w > 0 && bg_h > 0 {
                let bg = Rectangle16 {
                    x: i16::try_from(x).unwrap_or(i16::MAX),
                    y: i16::try_from(y - ascent).unwrap_or(i16::MAX),
                    width: u16::try_from(bg_w).unwrap_or(u16::MAX),
                    height: u16::try_from(bg_h).unwrap_or(u16::MAX),
                };
                if let Some(target) = self.resolve_paint_target(host_xid) {
                    self.paint_solid_spans(origin, host_xid, target, background, vec![bg]);
                }
            }
        }
        let result = self.render_text_chars(origin, host_xid, foreground, x, y, chars);
        self.core.current_function = saved_function;
        result
    }
}

/// Sum of character advances for a run, with the X11 nonexistent-
/// char rule: missing chars use the font's default_char; if that's
/// missing too they contribute 0 (drawn as nothing).
pub(in crate::kms::render::backend) fn text_advance(
    fs: &crate::kms::core::FontState,
    chars: &[char],
) -> i32 {
    let default_ci =
        char::from_u32(u32::from(fs.metrics.default_char)).and_then(|d| fs.char_info_cache.get(&d));
    chars
        .iter()
        .map(|ch| {
            fs.char_info_cache
                .get(ch)
                .or(default_ci)
                .map_or(0, |ci| i32::from(ci.character_width))
        })
        .sum()
}

/// Rasterize one glyph into monochrome horizontal spans positioned
/// relative to the text baseline origin `(0, 0)`.
fn rasterize_glyph_mono_spans(face: &mut freetype::Face, ch: char) -> Vec<Rectangle16> {
    let _ = face.load_char(
        ch as usize,
        freetype::face::LoadFlag::RENDER | freetype::face::LoadFlag::TARGET_MONO,
    );
    let glyph = face.glyph();
    let mut spans = Vec::new();
    glyph_mono_spans(
        &glyph.bitmap(),
        glyph.bitmap_left(),
        -glyph.bitmap_top(),
        &mut spans,
    );
    spans
}

/// Translate cached glyph-local spans into a concrete text run at the
/// baseline origin `(base_x, base_y)`.
fn translate_glyph_spans(
    glyph_spans: &[Rectangle16],
    base_x: i32,
    base_y: i32,
    out: &mut Vec<Rectangle16>,
) {
    for span in glyph_spans {
        let x = base_x + i32::from(span.x);
        let y = base_y + i32::from(span.y);
        if x < i32::from(i16::MIN)
            || x > i32::from(i16::MAX)
            || y < i32::from(i16::MIN)
            || y > i32::from(i16::MAX)
        {
            continue;
        }
        out.push(Rectangle16 {
            x: x as i16,
            y: y as i16,
            width: span.width,
            height: span.height,
        });
    }
}

/// Convert one rasterized glyph bitmap into horizontal pixel-run
/// rectangles at (ox, oy), appended to `out`. Handles FreeType MONO
/// (1 bpp, MSB-first, `pitch` bytes/row) and GRAY (8 bpp, threshold
/// at 128 — only reachable if a driver ignores TARGET_MONO) pixel
/// modes. Coordinates outside i16 range are clamped away (X11
/// drawables can't exceed i16 anyway).
fn glyph_mono_spans(bitmap: &freetype::Bitmap, ox: i32, oy: i32, out: &mut Vec<Rectangle16>) {
    let w = bitmap.width() as usize;
    let h = bitmap.rows() as usize;
    if w == 0 || h == 0 {
        return;
    }
    let pitch = bitmap.pitch();
    let buf = bitmap.buffer();
    let mono = matches!(bitmap.pixel_mode(), Ok(freetype::bitmap::PixelMode::Mono));
    for row in 0..h {
        let row_start = if pitch >= 0 {
            row * pitch as usize
        } else {
            (h - 1 - row) * (pitch as isize).unsigned_abs()
        };
        let set_at = |col: usize| -> bool {
            if mono {
                let byte = buf.get(row_start + (col >> 3)).copied().unwrap_or(0);
                byte & (0x80 >> (col & 7)) != 0
            } else {
                buf.get(row_start + col).copied().unwrap_or(0) >= 128
            }
        };
        let y = oy + row as i32;
        if y < i32::from(i16::MIN) || y > i32::from(i16::MAX) {
            continue;
        }
        let mut col = 0usize;
        while col < w {
            if !set_at(col) {
                col += 1;
                continue;
            }
            let run_start = col;
            while col < w && set_at(col) {
                col += 1;
            }
            let x = ox + run_start as i32;
            let width = (col - run_start) as u32;
            if x > i32::from(i16::MAX) || x + width as i32 <= i32::from(i16::MIN) {
                continue;
            }
            out.push(Rectangle16 {
                x: i16::try_from(x).unwrap_or(i16::MIN),
                y: y as i16,
                width: u16::try_from(width).unwrap_or(u16::MAX),
                height: 1,
            });
        }
    }
}

/// Pass 1 of the `CompositeGlyphs` items parse — mirrors v1's
/// `try_vk_render_composite_glyphs` shape.
///
/// Element size depends on the minor opcode: `CompositeGlyphs8` (23)
/// → 1-byte ids, 16 (24) → 2, 32 (25) → 4. Each element starts with
/// `count(u8) pad pad pad dx(i16) dy(i16)`; if `count == 255` the
/// same 8 bytes instead carry an inline **glyphset change**, with the
/// new glyphset xid in the trailing u32. `x_off` / `y_off` seed the
/// pen and each element's `dx` / `dy` accumulates onto it.
///
/// That inline form is why one request can mix glyph source formats:
/// different glyphsets may have different picture formats, so the
/// returned sequence can interleave A8 and ARGB32 glyphs — which the
/// engine then has to record as several contiguous draw runs
/// (`RenderEngine::split_glyph_runs`).
///
/// Split out of `render_composite_glyphs` for two reasons: it keeps
/// the immutable `glyphsets` borrow off the `&mut self.engine` call
/// that consumes the result, and it is the seam a test drives to
/// assert that the glyphs — and their source-format tags — come out
/// in request order across an inline glyphset change.
pub(in crate::kms::render) fn parse_composite_glyph_items(
    glyphsets: &HashMap<u32, crate::kms::core::GlyphSetState>,
    minor: u8,
    host_gs: u32,
    x_off: i16,
    y_off: i16,
    items: &[u8],
) -> ParsedGlyphItems {
    use crate::kms::core::GlyphSetFormat;

    let id_size: usize = match minor {
        23 => 1,
        24 => 2,
        _ => 4,
    };
    let mut out = ParsedGlyphItems::default();
    let mut pen_x = i32::from(x_off);
    let mut pen_y = i32::from(y_off);
    let mut pos: usize = 0;
    let mut active_gs_xid = host_gs;
    while pos + 8 <= items.len() {
        let count = items[pos] as usize;
        if count == 255 {
            let new_xid = u32::from_le_bytes([
                items[pos + 4],
                items[pos + 5],
                items[pos + 6],
                items[pos + 7],
            ]);
            if new_xid != 0 && glyphsets.contains_key(&new_xid) {
                active_gs_xid = new_xid;
            }
            pos += 8;
            continue;
        }
        out.elements += 1;
        let dx = i32::from(i16::from_le_bytes([items[pos + 4], items[pos + 5]]));
        let dy = i32::from(i16::from_le_bytes([items[pos + 6], items[pos + 7]]));
        pen_x += dx;
        pen_y += dy;

        let payload_start = pos + 8;
        let payload_bytes = count * id_size;
        let padded = (payload_bytes + 3) & !3;
        if payload_start + padded > items.len() {
            break;
        }

        let Some(active_gs) = glyphsets.get(&active_gs_xid) else {
            pos += 8 + padded;
            continue;
        };
        let active_gs_xid_for_key = active_gs_xid;

        for i in 0..count {
            let id_off = payload_start + i * id_size;
            let glyph_id: u32 = match id_size {
                1 => u32::from(items[id_off]),
                2 => u32::from(u16::from_le_bytes([items[id_off], items[id_off + 1]])),
                _ => u32::from_le_bytes([
                    items[id_off],
                    items[id_off + 1],
                    items[id_off + 2],
                    items[id_off + 3],
                ]),
            };
            let Some(glyph) = active_gs.glyphs.get(&glyph_id) else {
                out.missing += 1;
                continue;
            };
            out.found += 1;

            let gw = u32::from(glyph.width);
            let gh = u32::from(glyph.height);
            let dst_x = pen_x - i32::from(glyph.x);
            let dst_y = pen_y - i32::from(glyph.y);

            if gw > 0 && gh > 0 {
                let source_format = match glyph.format {
                    GlyphSetFormat::A8 => GlyphSourceFormat::A8,
                    // Wire A1: rows padded to a 32-bit scanline
                    // unit, bit order = advertised `bitmap-bit-order`
                    // (LSBFirst for the common little-endian client).
                    // Forwarded raw; expanded on atlas miss by
                    // `GlyphPixels::to_a8`.
                    GlyphSetFormat::A1 => GlyphSourceFormat::A1,
                    // Wire ARGB32: dense CARD32 rows, memory order
                    // [B, G, R, A]. Forwarded raw; reduced to one A8
                    // coverage plane on atlas miss by
                    // `GlyphPixels::to_a8`. Reducing here instead
                    // would throw away the colour channels a
                    // subpixel-AA client puts the coverage in.
                    GlyphSetFormat::Argb32 => GlyphSourceFormat::Argb32,
                    // A glyphset whose picture format we never
                    // accepted; `parse_add_glyphs` refuses to store
                    // glyphs for it, so this is defensive.
                    GlyphSetFormat::Other => {
                        log::warn!(
                            "render composite_glyphs: unexpected stored format {:?} for \
                             glyph 0x{glyph_id:x} — skipping",
                            glyph.format,
                        );
                        continue;
                    }
                };

                out.glyphs.push(ParsedGlyph {
                    gs_xid: active_gs_xid_for_key,
                    glyph_id,
                    w: gw,
                    h: gh,
                    source_format,
                    dst_x,
                    dst_y,
                });
            }

            pen_x += i32::from(glyph.x_off);
            pen_y += i32::from(glyph.y_off);
        }

        pos += 8 + padded;
    }
    out
}
