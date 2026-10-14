//! What the core thread asks of a window shell.

use std::sync::Arc;

/// An X cursor image for the window system's cursor.
#[derive(Debug, Clone)]
pub(crate) struct CursorImage {
    /// The cursor record's version: equal versions are equal images.
    pub(crate) version: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) hot: (u16, u16),
    /// Premultiplied BGRA, `width * height * 4` bytes.
    pub(crate) bgra: Arc<[u8]>,
}

impl CursorImage {
    /// The pixels as premultiplied RGBA.
    pub(crate) fn rgba(&self) -> Vec<u8> {
        let mut rgba = self.bgra.to_vec();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        rgba
    }
}

/// The shell's UI commands; each runs on the shell's UI thread.
pub(crate) trait ShellUi: Send + Sync {
    /// Show `title` on the window.
    #[cfg_attr(kms, allow(dead_code))] // Called by `server`, built without KMS.
    fn set_title(&self, title: &str);
    /// The core loop has ended and released every Vulkan object; quit.
    #[cfg_attr(kms, allow(dead_code))] // Called by `server`, built without KMS.
    fn quit(&self, failed: bool);
    /// Show `cursor` over the window, or no cursor.
    fn set_cursor(&self, cursor: Option<CursorImage>);
    /// Move the host pointer to X pixel `(x, y)` of the window.
    fn warp_pointer(&self, x: i32, y: i32);
    /// Size the window's content to `width`x`height` X pixels; the resize
    /// event that follows carries `token` (`ShellEvent::Resized`).
    fn set_content_size(&self, width: u16, height: u16, token: u64);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CursorImage;

    #[test]
    fn rgba_swaps_blue_and_red_only() {
        let image = CursorImage {
            version: 1,
            width: 2,
            height: 1,
            hot: (0, 0),
            bgra: Arc::from([1u8, 2, 3, 4, 5, 6, 7, 8].as_slice()),
        };
        assert_eq!(image.rgba(), [3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
