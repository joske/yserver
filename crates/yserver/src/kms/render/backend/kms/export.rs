use super::*;

/// Client GLX vendor library names to advertise for a given render
/// driver, in libglvnd priority order.
///
/// Same shape as the other per-driver policies in this tree —
/// `scanout_prefers_linear` (`kms/vk/scanout.rs:922`). DRI3 syncobj
/// used to be one of them (`VkContext::supports_dri3_syncobj`), but it
/// is a kernel capability now: `DRM_CAP_SYNCOBJ_TIMELINE` on the render
/// node (`RenderDevice::syncobj_timeline`), not a driver branch.
///
/// Deliberately binary. Every non-NVIDIA driver keeps "mesa": an
/// unmeasured mapping that redirects a configuration working today onto
/// a nonexistent `libGLX_*.so` is worse than the status quo, and nobody
/// working on this repo can measure AMD proprietary, Imagination, or
/// the rest.
///
/// The second entry matters on NVIDIA. If the NVIDIA Vulkan ICD is
/// installed but `libGLX_nvidia.so` is not — a routine package split —
/// a bare "nvidia" leaves libglvnd with no vendor and it lands on
/// `FALLBACK_VENDOR_NAME` "indirect", worse than today's llvmpipe.
pub(in crate::kms::render::backend) fn glx_vendor_names_for_driver(
    driver_id: ash::vk::DriverId,
) -> &'static str {
    if matches!(driver_id, ash::vk::DriverId::NVIDIA_PROPRIETARY) {
        "nvidia mesa"
    } else {
        yserver_protocol::x11::glx::VENDOR_NAMES
    }
}

pub(in crate::kms::render::backend) fn probe_dmabuf_export_support(
    vk: &std::sync::Arc<crate::kms::vk::device::VkContext>,
) -> bool {
    use crate::kms::vk::{
        dri3::export_backing,
        target::{EXPORT_FORMAT_BGRA8, allocate_exportable},
    };
    // NVIDIA proprietary: server-side export succeeds, but the NVIDIA GL
    // driver on the *client* side cannot import our Vulkan-exported dma-bufs
    // for GLX_EXT_texture_from_pixmap — every TFP-bound texture samples black
    // (HW-confirmed, GTX 1050; see project_nvidia_tfp_black_windows). The
    // only consumer of this probe is the TFP-extension advertisement, so
    // report unsupported here: compositors then take their pre-TFP fallback
    // (read pixmap → glTexImage2D), which renders correctly. yserver worked
    // this way before TFP existed.
    if matches!(vk.driver_id, ash::vk::DriverId::NVIDIA_PROPRIETARY) {
        return false;
    }
    let Ok(img) = allocate_exportable(vk, 1, 1, EXPORT_FORMAT_BGRA8) else {
        return false;
    };
    export_backing(vk, &img).is_ok()
    // `img` drops here → `ExportableImage::Drop` frees VkImage + VkDeviceMemory.
}

impl KmsBackend {
    // ── GLX-TFP (Tasks 2.3 + 2.4): exported-backing lifetime ─────────

    /// Ensure an `ExportedBacking` entry exists for `id`, taking the
    /// single backing lifetime ref on first creation. Idempotent.
    ///
    /// Spelled as an explicit `contains_key` → take-ref → `insert` →
    /// `get_mut` rather than `entry().or_insert_with(..)` because taking
    /// the lifetime ref needs `&mut self` (alias_registry / store), which
    /// would conflict with an `entry` borrow of `self.exported_dmabufs`.
    pub(in crate::kms::render::backend) fn ensure_exported_entry(
        &mut self,
        id: crate::kms::render::store::DrawableId,
        backing: PixmapHandle,
    ) -> &mut ExportedBacking {
        if !self.exported_dmabufs.contains_key(&id) {
            // ONE lifetime ref per backing, released exactly once in teardown.
            let via_alias = self.take_backing_lifetime_ref(id, backing);
            self.exported_dmabufs.insert(
                id,
                ExportedBacking {
                    fd: None,
                    glx_refs: 0,
                    lifetime_ref_held: true,
                    backing_id: id,
                    backing,
                    lifetime_via_alias: via_alias,
                },
            );
        }
        self.exported_dmabufs
            .get_mut(&id)
            .expect("entry just ensured")
    }

    /// `export holders` rows: drawables in exportable memory or with an export entry, plus orphaned entries.
    pub(crate) fn export_holder_rows(&self) -> Vec<crate::kms::render::export_holders::HolderRow> {
        use crate::kms::{
            render::export_holders::{ExportRow, HolderRow, StoreRow},
            vk::mem_accounting::{self, MemCategory},
        };
        let export_row = |e: &ExportedBacking| ExportRow {
            glx_refs: e.glx_refs,
            dri3_fd: e.fd.is_some(),
            lifetime_held: e.lifetime_ref_held,
            lifetime_via_alias: e.lifetime_via_alias,
        };
        let mut pictures: HashMap<crate::kms::render::store::DrawableId, u32> = HashMap::new();
        for id in self.picture_drawable_ids.values() {
            *pictures.entry(*id).or_default() += 1;
        }
        let redirect_of: HashMap<u32, u32> = self
            .core
            .host_window_to_backing
            .iter()
            .map(|(&w, b)| (b.as_raw(), w))
            .collect();
        let mut rows = Vec::new();
        for d in self.store.drawables() {
            let entry = mem_accounting::entry_of(d.storage.memory);
            let export = self.exported_dmabufs.get(&d.id);
            let export_mem = matches!(
                entry,
                Some((_, MemCategory::RedirectExport | MemCategory::TfpExport))
            );
            if !export_mem && export.is_none() {
                continue;
            }
            let attached = self.store.lookup(d.xid) == Some(d.id);
            let handle = PixmapHandle::from_raw(d.xid);
            rows.push(HolderRow {
                host_xid: d.xid,
                store: Some(StoreRow {
                    drawable_id: d.id.as_u64(),
                    category: entry.map(|(_, c)| c),
                    width: d.storage.extent.width,
                    height: d.storage.extent.height,
                    bytes: entry.map_or(0, |(b, _)| b),
                    refcount: d.refcount,
                    pending_retire: self.store.is_pending_retire(d.id),
                    xid_attached: attached,
                    pictures: pictures.get(&d.id).copied().unwrap_or(0),
                }),
                alias_refcount: handle
                    .filter(|_| attached)
                    .and_then(|h| self.core.alias_registry.get(h))
                    .map(|a| a.refcount),
                export: export.map(export_row),
                sync_dup: self.store.is_exported(d.id),
                redirect_of: redirect_of.get(&d.xid).copied().filter(|_| attached),
            });
        }
        for (id, e) in &self.exported_dmabufs {
            if self.store.get(*id).is_none() {
                rows.push(HolderRow {
                    host_xid: e.backing.as_raw(),
                    store: None,
                    alias_refcount: None,
                    export: Some(export_row(e)),
                    sync_dup: self.store.is_exported(*id),
                    redirect_of: None,
                });
            }
        }
        rows
    }

    /// GLX-TFP (Task 3.4 callers): record that a GLXPixmap now references
    /// the export of `host_xid`. Creates the entry (+ lifetime ref) if
    /// this is the first arrival. Public so the GLX protocol surface can
    /// call it. No-op on an unknown xid.
    pub fn acquire_glx_pixmap_export(&mut self, host_xid: u32) {
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        let Some(backing) = PixmapHandle::from_raw(host_xid) else {
            return;
        };
        let entry = self.ensure_exported_entry(id, backing);
        entry.glx_refs += 1;
    }

    /// GLX-TFP (Task 3.4 callers): a GLXPixmap referencing the export of
    /// `host_xid` is gone. Drops one `glx_refs` and tears the export down
    /// when it reaches zero.
    pub fn release_glx_pixmap_export(&mut self, host_xid: u32) {
        let Some(id) = self.store.lookup(host_xid) else {
            return;
        };
        if let Some(e) = self.exported_dmabufs.get_mut(&id) {
            e.glx_refs = e.glx_refs.saturating_sub(1);
        }
        self.maybe_teardown_export(id);
    }

    /// GLX-TFP: tear the export down iff no GLX consumer references it
    /// (`glx_refs == 0`). Drops the dup'd fd, clears the store's sync-fd
    /// dup, and releases the single lifetime ref. While `glx_refs > 0`
    /// this is a no-op (defer-destroy-while-referenced). There is NO
    /// `x_pixmap_freed` flag — an early `FreePixmap` is bridged by the
    /// lifetime ref and resolved here once `glx_refs` hits 0.
    pub(in crate::kms::render::backend) fn maybe_teardown_export(
        &mut self,
        id: crate::kms::render::store::DrawableId,
    ) {
        let ready = matches!(self.exported_dmabufs.get(&id), Some(e) if e.glx_refs == 0);
        if !ready {
            return;
        }
        if let Some(e) = self.exported_dmabufs.remove(&id) {
            // Drop the store's parallel sync-fd dup so no write can be
            // routed through a torn-down export.
            self.store.clear_exported_sync_fd(id);
            // `e.fd` (our dup) closes on drop; the kernel buffer survives
            // while any GL consumer still holds its own fd.
            if e.lifetime_ref_held {
                self.release_backing_lifetime_ref(&e);
            }
        }
    }

    /// GLX-TFP (Task 3.5 `glXBindTexImageEXT`): promote the backing for
    /// `host_xid` to dma-buf-exportable storage, idempotently, WITHOUT
    /// touching `glx_refs` or `exported_dmabufs` (no fd alloc/export).
    /// Returns whether the backing is exportable after the call.
    ///
    /// This is the lightweight bind hook — distinct from
    /// `acquire_glx_pixmap_export` (which manages the GLXPixmap LIFETIME
    /// ref). Binds happen repeatedly (per-frame); coupling them to the
    /// lifetime refcount would leak the backing. The engine's
    /// `promote_drawable_exportable` short-circuits via `is_exportable()`
    /// so repeated binds cost nothing after the first.
    pub fn promote_pixmap_exportable(&mut self, host_xid: u32) -> bool {
        let Some(id) = self.store.lookup(host_xid) else {
            return false;
        };
        // Already exportable → idempotent no-op (the common rebind case).
        if self
            .store
            .get(id)
            .map(|d| d.storage.is_exportable())
            .unwrap_or(false)
        {
            return true;
        }
        // First bind: promote. Requires Vulkan; if unavailable, report
        // not-exportable rather than erroring (bind still succeeds at the
        // protocol layer — see process_request's BindTexImageEXT TODO).
        if self.platform.vk.is_none() {
            return false;
        }
        // Real promotion (past the is_exportable no-op above) → 1 SyncBoundary
        // flush inside engine.promote_drawable_exportable. Counted for the
        // gkrellm submit-storm attribution (project_client_scheduling_fairness).
        self.telemetry.record_promote_exportable_run();
        match self
            .engine
            .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
        {
            Ok(()) => self
                .store
                .get(id)
                .map(|d| d.storage.is_exportable())
                .unwrap_or(false),
            Err(e) => {
                log::warn!("GLX BindTexImageEXT promote 0x{host_xid:x} failed: {e:?}");
                false
            }
        }
    }

    /// GLX-TFP test introspection: true iff `host_xid` currently has an
    /// `ExportedBacking` entry.
    #[doc(hidden)]
    pub fn has_export_entry(&self, host_xid: u32) -> bool {
        self.store
            .lookup(host_xid)
            .is_some_and(|id| self.exported_dmabufs.contains_key(&id))
    }
}

/// DRI3 version for a given syncobj capability. 1.4 is the version carrying
/// `ImportSyncobj` / `FreeSyncobj`; without them the server caps at 1.3 and
/// clients fall back to the fence path.
pub(in crate::kms::render::backend) fn dri3_version_for(syncobj: bool) -> (u32, u32) {
    if syncobj { (1, 4) } else { (1, 3) }
}

/// Whether the selected renderer can provide DRI3 import.
///
/// Implicit-LINEAR imports are validated against the selected renderer's
/// queried Vulkan layout for every buffer, including its offset and pitch.
/// The presence of another render-capable GPU therefore cannot make an import
/// unsafe; an incompatible foreign PRIME buffer is rejected at that boundary.
/// An unverified selected renderer still cannot coalesce multiple KMS devices
/// safely, so retain the historical one-KMS allowance for that fallback.
pub(in crate::kms::render::backend) fn dri3_import_supported_for_topology(
    selected_renderer: RenderDeviceId,
    kms_device_count: usize,
) -> bool {
    selected_renderer != RenderDeviceId::UnverifiedFallback || kms_device_count <= 1
}

impl KmsBackend {
    pub(in crate::kms::render::backend) fn backend_export_report_export_holders(
        &mut self,
        core: &dyn Fn() -> yserver_core::backend::export_holders::CoreHolders,
    ) -> bool {
        let rows = self.export_holder_rows();
        if !self.export_holders.observe(rows) {
            return false;
        }
        let core = core();
        for line in
            crate::kms::render::export_holders::format_report(self.export_holders.rows(), &core)
        {
            log::info!(target: crate::RESOURCE_TELEMETRY_TARGET, "{line}");
        }
        true
    }

    pub(in crate::kms::render::backend) fn backend_export_glx_vendor_names(&self) -> &'static str {
        self.platform
            .vk
            .as_ref()
            .map_or(yserver_protocol::x11::glx::VENDOR_NAMES, |vk| {
                glx_vendor_names_for_driver(vk.driver_id)
            })
    }

    // ── DRI3 — ported from v1 (Stage 4d backfill) ───────────────
    //
    // Body shape mirrors `kms/backend.rs:8613-8869` verbatim — the
    // helpers in `kms::vk::dri3`, `kms::vk::sync`, `kms::render_node`,
    // and `kms::xshmfence` are already shared with v1, so v2 calls
    // them directly. Without these, no compositor (marco, xfwm4,
    // picom, compton) can import redirected window backings as GPU
    // textures and the 4d-close hardware smoke wedges on
    // PresentPixmap → COW.
    pub(in crate::kms::render::backend) fn backend_export_dri3_open(
        &mut self,
        _drawable: u32,
    ) -> io::Result<std::os::fd::OwnedFd> {
        // Open a fresh fd at the render-node path per client. dup()'ing
        // a shared long-lived fd would give every client the same
        // kernel struct file, and libdrm_amdgpu maintains GEM handles
        // + contexts in per-struct-file state — the first client
        // populates it, the second crashes in `amdgpu_winsys_create`
        // hitting leftover handles. See
        // feedback_dri3_open_fresh_fd.md.
        let render_node = self
            .platform
            .selected_render_device()
            .and_then(|device| device.render_node.as_ref())
            .ok_or_else(|| {
                io::Error::other("DRI3 unavailable — render node was not resolved at backend init")
            })?;
        render_node.open_fresh().map_err(|e| {
            io::Error::other(format!(
                "open render-node {}: {e}",
                render_node.path().display()
            ))
        })
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_capabilities(&self) -> Dri3Caps {
        // DRI3 entirely unavailable when the render-node device or Vulkan
        // weren't resolved at backend init: pixmap import/export still needs
        // both. `render_node_device` is the guard here because it is what the syncobj ioctls and the
        // capability query run on.
        let Some(renderer) = self.platform.selected_render_device() else {
            return Dri3Caps::unsupported();
        };
        if renderer.render_node_device.is_none() || self.platform.vk.is_none() {
            return Dri3Caps::unsupported();
        }
        let vk = self.platform.vk.as_ref().expect("vk Some by branch above");
        // PixmapFromBuffer carries a client-chosen stride. The implicit-linear
        // import path validates it against the exact Vulkan layout per buffer;
        // a second PRIME renderer is therefore not a reason to hide DRI3.
        if !dri3_import_supported_for_topology(renderer.id, self.platform.devices.len()) {
            return Dri3Caps::unsupported();
        }
        let modifiers = vk.image_drm_format_modifier;
        // FenceFromFD / FDFromFence need SYNC_FD semaphore import/export.
        let fence_fd = vk.supports_sync_fd();
        // Syncobj support is a property of the KERNEL, not of the Vulkan
        // driver. The previous NVIDIA blacklist here was a correct response
        // to vkImportSemaphoreFdKHR rejecting DRM syncobj fds, which no
        // longer matters because nothing imports them into Vulkan.
        let syncobj = renderer.syncobj_timeline;
        Dri3Caps {
            version: dri3_version_for(syncobj),
            modifiers,
            fence_fd,
            syncobj,
        }
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_import_pixmap(
        &mut self,
        fd: std::os::fd::OwnedFd,
        width: u16,
        height: u16,
        stride: u32,
        offset: u32,
        modifier: Dri3ImportModifier,
        depth: u8,
        bpp: u8,
    ) -> io::Result<PixmapHandle> {
        // Per Phase 4.2 design §3.2: import the dma-buf into a
        // DrawableImage via VK_EXT_image_drm_format_modifier, wrap
        // it as a v2 Storage, allocate a fresh Pixmap entry in
        // the store. Pixmap exists as a real X resource so clients
        // can CopyArea / ChangePicture against it.
        let Some(vk) = self.platform.vk.clone() else {
            return Err(io::Error::other("DRI3 import: Vulkan unavailable"));
        };
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => {
                return Err(io::Error::other(format!(
                    "DRI3 import: unsupported (depth={depth}, bpp={bpp}); Phase 4.2 RGB single-plane only"
                )));
            }
        };
        // Vulkan's modifier import is explicit-only, so an implicit
        // layout has to be resolved to a concrete modifier first. gbm
        // does that the way glamor does it for the same request
        // (`gbm_bo_import(GBM_BO_IMPORT_FD)` then `gbm_bo_get_modifier`,
        // ../xserver/glamor/glamor_egl.c:572 and :450).
        //
        // On failure this returns Err rather than falling back to
        // LINEAR. Guessing is what #138 was: a wrong *successful*
        // import corrupts the client's own output and reports success,
        // where a refused one is visible and debuggable.
        // What we tell clients later, which is the half that matters:
        // `DRM_FORMAT_MOD_INVALID` means "layout not named", and a client
        // re-importing on that answer resolves it itself (EGL and GL have
        // an implicit dma-buf import path; Vulkan does not). Claiming
        // LINEAR instead is #138 -- the client believes us and samples a
        // tiled buffer as linear.
        //
        // We cannot do better than "unknown" here: gbm reports
        // DRM_FORMAT_MOD_INVALID for an implicitly imported buffer on
        // amdgpu -- measured, and it does so even for gbm's own fresh
        // allocation -- so there is nothing to resolve against. i915 does
        // report a concrete modifier, but a fix that only works on Intel
        // is not a fix.
        let (vk_modifier, reported_modifier, client_size, implicit_layout) = match modifier {
            Dri3ImportModifier::Explicit(m) => (m, Some(m), None, false),
            // LINEAR is a best-effort for OUR OWN Vulkan view of the
            // buffer, which is only ever sampled if the server itself
            // composites this pixmap. It is deliberately NOT what we
            // report back.
            Dri3ImportModifier::Implicit { size } => (
                crate::kms::vk::dri3::DRM_FORMAT_MOD_LINEAR,
                Some(crate::kms::vk::dri3::DRM_FORMAT_MOD_INVALID),
                Some(size),
                true,
            ),
        };
        let modifier = vk_modifier;
        let drawable = crate::kms::vk::dri3::import_dmabuf_reporting(
            vk.clone(),
            fd,
            u32::from(width),
            u32::from(height),
            format,
            modifier,
            reported_modifier,
            client_size,
            &[crate::kms::vk::dri3::DmabufPlane {
                offset: u64::from(offset),
                pitch: stride,
            }],
        )
        .map_err(|e| io::Error::other(format!("DRI3 import_dmabuf: {e:?}")))?;
        // Build a sample-side view over the imported VkImage. The
        // DRI3 path's own `vk_image_view` (kept as `image_view` on
        // the resulting Storage) is IDENTITY-swizzle and serves as
        // the attachment view; the sample-side view applies the
        // format/depth-aware swizzle the scene compositor relies on
        // (depth-24 BGRA8 → α=ONE).
        let sample_view = crate::kms::render::platform::PlatformBackend::build_sample_view(
            &vk,
            drawable.vk_image,
            drawable.format,
            depth,
        )
        .map_err(|e| io::Error::other(format!("DRI3 import build_sample_view: {e:?}")))?;
        let fourcc = match depth {
            24 => u32::from_le_bytes(*b"XR24"),
            32 => u32::from_le_bytes(*b"AR24"),
            _ => unreachable!("format match above restricts imported depth"),
        };
        let storage = Storage::from_imported_drawable_image(
            drawable,
            sample_view,
            depth,
            ImportedDmabufMetadata {
                fourcc,
                vk_format: format,
                modifier,
                implicit_layout,
                planes: vec![ImportedDmabufPlane {
                    offset: u64::from(offset),
                    pitch: stride,
                }],
                width,
                height,
                depth,
                bpp,
            },
        );
        let host_xid = self.core.next_host_xid();
        self.store_alloc(host_xid, DrawableKind::Pixmap, depth, false, storage)
            .map_err(|e| io::Error::other(format!("DRI3 import store.allocate: {e:?}")))?;
        // Telemetry: an imported pixmap is still a fresh storage
        // entry + a view (the DrawableImage built one inside
        // from_dmabuf). Mirrors init_root_storage's accounting so
        // the per-second counters stay accurate under DRI3 traffic.
        self.telemetry.record_storage_allocation();
        self.telemetry.record_image_view_create();
        PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other("DRI3 import: failed to make PixmapHandle"))
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_supported_modifiers(
        &self,
        _window: u32,
        depth: u8,
        bpp: u8,
    ) -> (Vec<u64>, Vec<u64>) {
        let Some(vk) = self.platform.vk.as_ref() else {
            return (vec![0], vec![0]);
        };
        // Map (depth, bpp) to a vk::Format. Phase 4.2 RGB single-
        // plane scope means we only handle depth-24/32 BGRA today.
        let format = match (depth, bpp) {
            (24 | 32, 32) => ash::vk::Format::B8G8R8A8_UNORM,
            _ => return (vec![0], vec![0]),
        };
        // Client pixmaps are composited as sampled window textures, so
        // probe with the sampled client-import usage (keeps SAMPLED, which
        // correctly steers v3dv clients to a tiled modifier).
        let probed = crate::kms::vk::dri3::supported_modifiers_with_planes(
            vk,
            format,
            crate::kms::vk::dri3::CLIENT_IMPORT_USAGE,
        );
        let screen: Vec<u64> = probed.iter().map(|(m, _)| *m).collect();
        // The window list is the SINGLE-PLANE subset, not just LINEAR.
        //
        // `PixmapFromBuffers` import handles one plane today, so a
        // multi-plane layout offered here is one we then refuse with
        // BadAlloc: Mesa logs `dri3_alloc_render_buffer ... failed` and
        // the window renders nothing at all. On this AMD part three of
        // the six tiled modifiers carry DCC and need two planes (three
        // when retiled), which is what makes the naive "advertise
        // everything" version blank every GL client.
        //
        // Collapsing to LINEAR is wrong in the other direction: it is
        // not what the question means once a window is composited
        // rather than flipped, and composited is our default. Xorg
        // answers with the tiled set here too.
        //
        // This is NOT a fix for #138, and was briefly believed to be.
        // Offering the tiled single-plane set left that bug exactly as
        // it was: Chrome's hardware-decoded video arrives over
        // EGL/dma-buf from VA-API and never travels this path. Do not
        // reintroduce that claim.
        //
        // Plane count comes from `drmFormatModifierPlaneCount`, not from
        // decoding modifier bits, so the filter tracks whatever the
        // driver actually reports.
        let window: Vec<u64> = probed
            .iter()
            .filter(|(_, planes)| *planes == 1)
            .map(|(m, _)| *m)
            .collect();
        (window, screen)
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_export_pixmap(
        &mut self,
        host_xid: u32,
    ) -> io::Result<(u32, u16, u16, u16, u8, u8, std::os::fd::OwnedFd)> {
        // op 3 is the single-fd, no-modifier subset of op 8. Share the
        // promote+export path and drop the modifier/offset (op 3's reply
        // has no field for them). stride truncates CARD32 → CARD16.
        let e = self.dri3_export_pixmap_buffers(host_xid)?;
        Ok((
            e.size,
            e.width,
            e.height,
            u16::try_from(e.stride).unwrap_or(u16::MAX),
            e.depth,
            e.bpp,
            e.fd,
        ))
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_export_pixmap_buffers(
        &mut self,
        host_xid: u32,
    ) -> io::Result<Dri3PixmapExport> {
        // Resolve xid → DrawableId before any mutable borrows.
        let id = self.store.lookup(host_xid).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: unknown pixmap 0x{host_xid:x}"))
        })?;

        // Promote-if-needed: migrate server-owned pixmaps onto dma-buf-exportable
        // storage (glamor model). Idempotent — early-returns if already exportable.
        if !self
            .store
            .get(id)
            .map(|d| d.storage.is_exportable())
            .unwrap_or(false)
        {
            // promote_drawable_exportable needs &mut self.engine/platform/store,
            // so we must not hold any shared borrow across this call.
            if self.platform.vk.is_none() {
                return Err(io::Error::other("DRI3 export: Vulkan unavailable"));
            }
            self.engine
                .promote_drawable_exportable(&mut self.platform, &mut self.store, id)
                .map_err(|e| io::Error::other(format!("DRI3 export promote: {e:?}")))?;
        }

        // Re-fetch after promotion (storage has been swapped).
        let vk = self
            .platform
            .vk
            .as_ref()
            .ok_or_else(|| io::Error::other("DRI3 export: Vulkan unavailable"))?;
        let drawable = self.store.get(id).ok_or_else(|| {
            io::Error::other(format!("DRI3 export: store entry missing 0x{host_xid:x}"))
        })?;

        let (depth, width, height) = self
            .core
            .alias_registry
            .get(PixmapHandle::from_raw_panicking(host_xid))
            .map(|alias| (alias.depth, alias.width, alias.height))
            .unwrap_or_else(|| {
                (
                    drawable.depth,
                    u16::try_from(drawable.storage.extent.width).unwrap_or(u16::MAX),
                    u16::try_from(drawable.storage.extent.height).unwrap_or(u16::MAX),
                )
            });
        let bpp: u8 = match depth {
            24 | 32 => 32,
            4 | 8 => 8,
            d => d,
        };

        // Export: imported images go through the DrawableImage path; promoted /
        // server-owned images use export_promoted on the storage's raw memory
        // handle + stride/size carried from allocation-time layout query.
        let export = if let Some(imported) = drawable.storage.imported_drawable.as_ref() {
            crate::kms::vk::dri3::export_dmabuf(vk, imported)
                .map_err(|e| io::Error::other(format!("DRI3 export_dmabuf: {e:?}")))?
        } else {
            debug_assert!(
                drawable.storage.export_stride != 0 && drawable.storage.export_size != 0,
                "promoted storage missing export metadata (stride={} size={})",
                drawable.storage.export_stride,
                drawable.storage.export_size,
            );
            crate::kms::vk::dri3::export_promoted(
                vk,
                drawable.storage.memory,
                drawable.storage.export_stride,
                drawable.storage.export_size,
                drawable.storage.export_modifier,
            )
            .map_err(|e| io::Error::other(format!("DRI3 export_promoted: {e:?}")))?
        };

        // GLX-TFP (Tasks 2.3 + 2.4): record/refresh the export tracking
        // entry. Repeated exports (muffin re-exports per damage) reuse the
        // existing entry and its single lifetime ref — the fd dup + sync
        // tracking install only on the FIRST export. Always return the
        // ORIGINAL fd to the client.
        let backing = PixmapHandle::from_raw(host_xid)
            .ok_or_else(|| io::Error::other(format!("DRI3 export: bad xid 0x{host_xid:x}")))?;
        if self
            .exported_dmabufs
            .get(&id)
            .is_none_or(|e| e.fd.is_none())
        {
            let dup = export.fd.try_clone()?;
            // Parallel sync-only dup for the engine flush chokepoint.
            let sync_dup = std::sync::Arc::new(dup.try_clone()?);
            self.ensure_exported_entry(id, backing).fd = Some(dup);
            self.store.set_exported_sync_fd(id, sync_dup);
        }

        Ok(Dri3PixmapExport {
            size: export.size,
            width,
            height,
            stride: export.stride,
            offset: export.offset,
            depth,
            bpp,
            modifier: export.modifier,
            fd: export.fd,
        })
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_fence_from_fd(
        &mut self,
        fence_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        // Mesa's loader_dri3 sends an xshmfence (memfd + futex) —
        // try that path FIRST. vkImportSemaphoreFdKHR rejects
        // xshmfence fds because they aren't sync_file. Mmap first;
        // fall through to Vulkan import only if mmap fails (i.e.
        // the fd really is a sync_file).
        use std::os::fd::AsFd as _;
        if let Some(mapping) = crate::kms::xshmfence::FenceMapping::map(fd.as_fd()) {
            self.dri3_xshmfences
                .insert(fence_xid, std::sync::Arc::new(mapping));
            log::debug!("DRI3 FenceFromFD 0x{fence_xid:x}: imported as xshmfence");
            return Ok(());
        }
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other(
                "DRI3 FenceFromFD: fd isn't xshmfence and Vulkan is unavailable",
            ));
        };
        let semaphore = crate::kms::vk::sync::import_sync_file(vk, fd)
            .map_err(|e| io::Error::other(format!("import_sync_file: {e:?}")))?;
        let owned = std::sync::Arc::new(crate::kms::render::owned_semaphore::OwnedSemaphore::new(
            vk.clone(),
            semaphore,
        ));
        // Replacing an entry drops the previous Arc here; if no other
        // clone is outstanding, OwnedSemaphore::Drop calls
        // vkDestroySemaphore.
        let _ = self.dri3_sync_resources.insert(fence_xid, owned);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_trigger_fence(
        &mut self,
        fence_xid: u32,
    ) -> io::Result<()> {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.trigger();
            return Ok(());
        }
        // VkSemaphore-backed fences: signalling is done via queue
        // submit. (DRI3 1.4 syncobjs are the other path — DRM objects
        // signalled by the `SYNCOBJ_TIMELINE_SIGNAL` ioctls, never
        // Vulkan.) For Phase 4.2 first-cut Copy path the GPU work is
        // already serialized, so a server-only `triggered=true` mirror
        // is sufficient — no GPU operation needed here.
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_fence_triggered(
        &self,
        fence_xid: u32,
    ) -> Option<bool> {
        self.dri3_xshmfences
            .get(&fence_xid)
            .map(|mapping| mapping.query() != 0)
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_reset_fence(
        &mut self,
        fence_xid: u32,
    ) {
        if let Some(mapping) = self.dri3_xshmfences.get(&fence_xid) {
            mapping.reset();
        }
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_destroy_fence(
        &mut self,
        fence_xid: u32,
    ) {
        // Xorg `miSyncShmScreenDestroyFence`: trigger, then unmap. A
        // deferred Present completion holding an Arc clone keeps the
        // mapping alive until it lets go.
        if let Some(mapping) = self.dri3_xshmfences.remove(&fence_xid) {
            mapping.trigger();
        }
        self.dri3_sync_resources.remove(&fence_xid);
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_xshmfence_handle(
        &self,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>> {
        self.dri3_xshmfences
            .get(&fence_xid)
            .cloned()
            .map(|arc| arc as std::sync::Arc<dyn yserver_core::backend::XshmfenceHandle>)
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_syncobj_handle(
        &self,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>> {
        self.dri3_syncobjs
            .get(&syncobj_xid)
            .map(|(_, arc)| arc.clone())
            .map(|arc| arc as std::sync::Arc<dyn yserver_core::backend::SyncobjHandle>)
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_syncobj_owned(
        &self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> bool {
        self.dri3_syncobjs
            .get(&syncobj_xid)
            .is_some_and(|(owner, _)| *owner == client_id)
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_fd_from_fence(
        &mut self,
        fence_xid: u32,
    ) -> io::Result<std::os::fd::OwnedFd> {
        let arc = self
            .dri3_sync_resources
            .get(&fence_xid)
            .cloned()
            .ok_or_else(|| {
                io::Error::other(format!("DRI3 FDFromFence: unknown fence 0x{fence_xid:x}"))
            })?;
        let Some(vk) = self.platform.vk.as_ref() else {
            return Err(io::Error::other("DRI3 FDFromFence: Vulkan unavailable"));
        };
        crate::kms::vk::sync::export_sync_file(vk, arc.semaphore())
            .map_err(|e| io::Error::other(format!("export_sync_file: {e:?}")))
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_import_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
        fd: std::os::fd::OwnedFd,
    ) -> io::Result<()> {
        use std::os::fd::AsFd;

        let render_node = self
            .platform
            .selected_render_device()
            .and_then(|device| device.render_node_device.as_ref())
            .cloned()
            .ok_or_else(|| {
                io::Error::other("DRI3 ImportSyncobj: render node not resolved at init")
            })?;
        if self.dri3_syncobjs.contains_key(&syncobj_xid) {
            return Err(io::Error::other(format!(
                "DRI3 ImportSyncobj: syncobj 0x{syncobj_xid:x} already imported"
            )));
        }
        let imported =
            crate::kms::render::imported_syncobj::ImportedSyncobj::import(render_node, fd.as_fd())?;
        // Arc Drop on any replaced entry destroys the previous handle.
        let _ = self
            .dri3_syncobjs
            .insert(syncobj_xid, (client_id, std::sync::Arc::new(imported)));
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_free_syncobj(
        &mut self,
        client_id: yserver_protocol::x11::ClientId,
        syncobj_xid: u32,
    ) -> io::Result<()> {
        let Some((owner, _)) = self.dri3_syncobjs.get(&syncobj_xid) else {
            return Err(io::Error::other(format!(
                "DRI3 FreeSyncobj: unknown syncobj 0x{syncobj_xid:x}"
            )));
        };
        if *owner != client_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("DRI3 FreeSyncobj: 0x{syncobj_xid:x} owned by another client"),
            ));
        }
        // Arc Drop destroys the DRM handle when the last reference goes away,
        // which may be later than this call: the deferred completion path
        // pins clones past FreeSyncobj.
        let _ = self.dri3_syncobjs.remove(&syncobj_xid);
        Ok(())
    }

    pub(in crate::kms::render::backend) fn backend_export_dri3_signal_syncobj(
        &mut self,
        syncobj_xid: u32,
        value: u64,
    ) -> io::Result<()> {
        use yserver_core::backend::SyncobjHandle as _;

        let arc = self
            .dri3_syncobjs
            .get(&syncobj_xid)
            .map(|(_, arc)| arc)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "DRI3 SignalSyncobj: unknown syncobj 0x{syncobj_xid:x}"
                ))
            })?;
        arc.signal(value)
    }
}
