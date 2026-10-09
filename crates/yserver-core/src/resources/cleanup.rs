use super::*;

impl ResourceTable {
    /// Per-type resource counts owned by `owner`, for X-Resource
    /// `QueryClientResources`. Returns `(type_name, count)` only for
    /// types with a non-zero count.
    ///
    /// Core type names (`WINDOW`/`PIXMAP`/`GC`/`FONT`/`CURSOR`/
    /// `COLORMAP`) are the canonical strings Xorg registers in
    /// `dix/registry.c`. `PICTURE`/`GLYPHSET` use descriptive names
    /// yserver chooses: Xorg does NOT register names for RENDER resource
    /// types (it returns the synthetic `"Unregistered resource N"` whose
    /// `N` is an Xorg-internal RESTYPE index we can't reproduce), so
    /// there is no canonical string to match. The X-Resource type name is
    /// a server-registered atom that clients resolve via `GetAtomName`,
    /// so a descriptive name is spec-legal and strictly more useful.
    #[must_use]
    pub fn resource_counts_by_owner(&self, owner: ClientId) -> Vec<(&'static str, u32)> {
        fn tally<V>(m: &HashMap<u32, V>, owner: ClientId, get: impl Fn(&V) -> ClientId) -> u32 {
            u32::try_from(m.values().filter(|v| get(v) == owner).count()).unwrap_or(u32::MAX)
        }
        [
            ("WINDOW", tally(&self.windows, owner, |w| w.owner)),
            ("PIXMAP", tally(&self.pixmaps, owner, |p| p.owner)),
            ("GC", tally(&self.gcs, owner, |g| g.owner)),
            ("FONT", tally(&self.fonts, owner, |f| f.owner)),
            ("CURSOR", tally(&self.cursors, owner, |c| c.owner)),
            ("COLORMAP", tally(&self.colormaps, owner, |c| c.owner)),
            ("PICTURE", tally(&self.pictures, owner, |p| p.client)),
            ("GLYPHSET", tally(&self.glyphsets, owner, |g| g.client)),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .collect()
    }

    /// Storage occupied by live pixmap resources owned by `owner`, for
    /// X-Resource `QueryClientPixmapBytes`. X11 pixmap scanlines are padded
    /// to 32 bits; depth 24 and 32 both use 32 storage bits per pixel.
    #[must_use]
    pub fn pixmap_bytes_by_owner(&self, owner: ClientId) -> u64 {
        self.pixmaps
            .values()
            .filter(|pixmap| pixmap.owner == owner)
            .map(|pixmap| {
                let bits_per_pixel = match pixmap.depth {
                    1 => 1u64,
                    2..=8 => 8,
                    9..=16 => 16,
                    _ => 32,
                };
                let row_bits = u64::from(pixmap.width).saturating_mul(bits_per_pixel);
                let row_bytes = row_bits.div_ceil(32).saturating_mul(4);
                row_bytes.saturating_mul(u64::from(pixmap.height))
            })
            .fold(0u64, u64::saturating_add)
    }

    /// Windows whose `attributes.colormap` references `cmap`. Used by
    /// `InstallColormap` / `UninstallColormap` to enumerate
    /// `ColormapNotify` recipients per X11 spec ("a ColormapNotify
    /// event is generated on every window having this colormap as
    /// an attribute").
    pub fn windows_with_colormap(&self, cmap: ResourceId) -> Vec<ResourceId> {
        let mut out = Vec::new();
        for (raw_id, w) in &self.windows {
            if w.colormap == cmap {
                out.push(ResourceId(*raw_id));
            }
        }
        out
    }

    /// Top-level windows owned by `client`: windows whose parent is *not*
    /// owned by the same client. Reachable descendants (regardless of
    /// owner) get destroyed transitively when each root is destroyed.
    pub fn collect_owned_window_roots(&self, client: ClientId, out: &mut Vec<ResourceId>) {
        for (raw_id, w) in &self.windows {
            if w.owner != client {
                continue;
            }
            let parent_owner = self.windows.get(&w.parent.0).map(|p| p.owner);
            if parent_owner != Some(client) {
                out.push(ResourceId(*raw_id));
            }
        }
    }

    /// Remove every non-window resource owned by `client`. Returns the
    /// `host_xid` of every removed font and every removed host-backed pixmap so
    /// the caller can issue host-side `CloseFont` / `FreePixmap` after dropping
    /// the `ServerState` lock.
    pub fn remove_non_window_resources_owned_by(
        &mut self,
        client: ClientId,
    ) -> ClientRemovedResources {
        let mut freed_pixmaps = Vec::new();
        let mut freed_names = Vec::new();
        let mut name_ids = Vec::new();
        self.pixmaps.retain(|_, p| {
            if p.owner == client {
                if p.composite_name {
                    name_ids.push(p.id);
                    freed_names.extend(p.host_xid);
                } else if let Some(xid) = p.host_xid {
                    freed_pixmaps.push(xid.as_raw());
                }
                false
            } else {
                true
            }
        });
        if !name_ids.is_empty() {
            self.forget_composite_names(&name_ids);
        }
        self.gcs.retain(|_, g| {
            if g.owner != client {
                return true;
            }
            freed_pixmaps.extend(
                g.held_host_pixmaps()
                    .into_iter()
                    .flatten()
                    .map(crate::backend::PixmapHandle::as_raw),
            );
            false
        });
        let mut freed_colormaps = Vec::new();
        self.colormaps.retain(|_, c| {
            if c.owner == client {
                if let Some(host) = c.host_colormap_xid {
                    freed_colormaps.push(host.as_raw());
                }
                false
            } else {
                true
            }
        });
        let mut freed_cursors = Vec::new();
        self.cursors.retain(|_, c| {
            if c.owner == client {
                if let Some(xid) = c.host_xid {
                    freed_cursors.push(xid.as_raw());
                }
                false
            } else {
                true
            }
        });
        // Host cursors shared with a surviving XID (XFIXES `ChangeCursor`
        // aliases) stay alive; each shared handle is released once.
        freed_cursors.sort_unstable();
        freed_cursors.dedup();
        freed_cursors.retain(|host| !self.cursor_host_referenced(*host));
        self.dropped_cursor_hosts
            .retain(|host| !freed_cursors.contains(host));
        let mut closed_fonts = Vec::new();
        self.fonts.retain(|_, f| {
            if f.owner == client {
                closed_fonts.push(f.host_xid.as_raw());
                false
            } else {
                true
            }
        });
        let mut freed_pictures: Vec<(u32, Option<u32>)> = Vec::new();
        self.pictures.retain(|_, p| {
            if p.client == client {
                if let Some(hp) = p.host_picture_xid {
                    freed_pictures.push((hp.as_raw(), p.host_owned_pixmap.map(|h| h.as_raw())));
                }
                false
            } else {
                true
            }
        });
        let mut freed_glyphsets = Vec::new();
        let removed_glyphsets = self
            .glyphsets
            .extract_if(|_, g| g.client == client)
            .map(|(_, g)| g.host_glyphset_xid.as_raw())
            .collect::<Vec<_>>();
        for host_xid in removed_glyphsets {
            if self.release_host_glyphset_ref(host_xid) {
                freed_glyphsets.push(host_xid);
            }
        }
        let freed_dri3_syncobjs = self
            .dri3_syncobjs
            .extract_if(|_, owner| *owner == client)
            .map(|(xid, _)| xid)
            .collect();
        ClientRemovedResources {
            closed_fonts,
            freed_pixmaps,
            freed_names,
            freed_pictures,
            freed_glyphsets,
            freed_cursors,
            freed_dri3_syncobjs,
        }
    }

    /// Look up the current owner of `id` across every core resource
    /// table. Returns `None` if the ID doesn't name any known resource.
    /// Server-static resources (root window, overlay, default colormap /
    /// visuals) carry `SERVER_OWNER`.
    #[must_use]
    pub fn resource_owner(&self, id: ResourceId) -> Option<ClientId> {
        if let Some(w) = self.windows.get(&id.0) {
            return Some(w.owner);
        }
        if let Some(p) = self.pixmaps.get(&id.0) {
            return Some(p.owner);
        }
        if let Some(g) = self.gcs.get(&id.0) {
            return Some(g.owner);
        }
        if let Some(f) = self.fonts.get(&id.0) {
            return Some(f.owner);
        }
        if let Some(c) = self.cursors.get(&id.0) {
            return Some(c.owner);
        }
        if let Some(p) = self.pictures.get(&id.0) {
            return Some(p.client);
        }
        if let Some(g) = self.glyphsets.get(&id.0) {
            return Some(g.client);
        }
        if let Some(owner) = self.dri3_syncobjs.get(&id.0) {
            return Some(*owner);
        }
        None
    }
}
