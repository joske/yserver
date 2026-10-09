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
