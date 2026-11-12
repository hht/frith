//! Skia on Metal for macOS windows: one device, command queue and Skia context shared by
//! every window, and one `CAMetalLayer` per window.
//!
//! Answers: how does a Skia frame reach an OS window?
//! Must not know: what is drawn or why.
//!
//! A single `DirectContext` serves all windows because everything renders on the main
//! thread, and sharing it lets glyph textures and shaders be reused across windows.

use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable,
    MTLPixelFormat,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin, backend_render_targets, mtl};
use skia_safe::{Canvas, ColorType, EncodedImageFormat};
use winit::raw_window_handle::RawWindowHandle;
use winit::window::Window;

pub struct Gpu {
    // Declared first so it drops before the device and queue it was created from.
    pub ctx: DirectContext,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

impl Gpu {
    pub fn new() -> Gpu {
        let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
        let queue = device.newCommandQueue().expect("Metal command queue");
        // SAFETY: both handles are live Metal objects that `Gpu` keeps alive until after
        // `ctx` has dropped (see field order).
        let backend = unsafe {
            mtl::BackendContext::new(
                Retained::as_ptr(&device) as mtl::Handle,
                Retained::as_ptr(&queue) as mtl::Handle,
            )
        };
        let ctx = gpu::direct_contexts::make_metal(&backend, None).expect("Skia Metal context");
        Gpu { ctx, device, queue }
    }
}

/// When the phases of one presented frame finished.
pub struct FrameTimes {
    /// A drawable was available (waiting for it is display pacing, not our cost).
    pub acquired: Instant,
    pub drawn: Instant,
    pub presented: Instant,
}

pub struct Surface {
    layer: Retained<CAMetalLayer>,
}

impl Surface {
    /// Backs `window`'s content view with a Metal layer. `vsync: false` presents as fast as
    /// frames are produced (benchmarking only).
    pub fn attach(gpu: &Gpu, window: &dyn Window, vsync: bool) -> Surface {
        let size = window.surface_size();
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&gpu.device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setPresentsWithTransaction(false);
        // Screenshots read the drawable back, which a framebuffer-only texture forbids.
        layer.setFramebufferOnly(false);
        layer.setDisplaySyncEnabled(vsync);
        layer.setDrawableSize(CGSize::new(size.width as f64, size.height as f64));
        let handle = window.rwh_06_window_handle().window_handle().expect("window handle");
        let RawWindowHandle::AppKit(h) = handle.as_raw() else { panic!("macOS only") };
        // SAFETY: winit guarantees `ns_view` is a valid NSView for the window's lifetime,
        // and this runs on the main thread.
        let view = unsafe { &*(h.ns_view.as_ptr() as *const NSView) };
        view.setWantsLayer(true);
        view.setLayer(Some(&layer.clone().into_super()));
        Surface { layer }
    }

    pub fn resize(&self, width: u32, height: u32) {
        self.layer.setDrawableSize(CGSize::new(width.max(1) as f64, height.max(1) as f64));
    }

    /// Draws one frame with `draw` and presents it. When `shot` is set, also writes the
    /// frame to that path as PNG. `sync` waits for the GPU so the present time includes
    /// GPU work. Returns `None` when no drawable is available (window hidden).
    pub fn present<R>(
        &self,
        gpu: &mut Gpu,
        sync: bool,
        shot: Option<&str>,
        draw: impl FnOnce(&Canvas, &mut DirectContext) -> R,
    ) -> Option<(R, FrameTimes)> {
        let drawable = self.layer.nextDrawable()?;
        let acquired = Instant::now();
        let size = self.layer.drawableSize();
        // SAFETY: the texture belongs to `drawable`, which outlives `surface` below.
        let tex = unsafe { mtl::TextureInfo::new(Retained::as_ptr(&drawable.texture()) as mtl::Handle) };
        let target = backend_render_targets::make_mtl((size.width as i32, size.height as i32), &tex);
        let mut surface = gpu::surfaces::wrap_backend_render_target(
            &mut gpu.ctx,
            &target,
            SurfaceOrigin::TopLeft,
            ColorType::BGRA8888,
            None,
            None,
        )
        .expect("wrap Metal drawable");
        let out = draw(surface.canvas(), &mut gpu.ctx);
        let drawn = Instant::now();
        if let Some(path) = shot {
            save_png(&mut surface, &mut gpu.ctx, path);
        }
        let submitted = if sync { gpu.ctx.flush_submit_and_sync_cpu() } else { gpu.ctx.flush_and_submit() };
        if let Err(e) = submitted {
            eprintln!("[gpu] submit failed: {e:?}");
        }
        drop(surface);
        let cb = gpu.queue.commandBuffer().expect("Metal command buffer");
        let d: Retained<ProtocolObject<dyn MTLDrawable>> = (&drawable).into();
        cb.presentDrawable(&d);
        cb.commit();
        Some((out, FrameTimes { acquired, drawn, presented: Instant::now() }))
    }
}

fn save_png(surface: &mut skia_safe::Surface, ctx: &mut DirectContext, path: &str) {
    let image = surface.image_snapshot();
    let png = image
        .make_raster_image(Some(&mut *ctx), None)
        .and_then(|raster| raster.encode(Some(&mut *ctx), EncodedImageFormat::PNG, None));
    match png.map(|png| std::fs::write(path, png.as_bytes())) {
        Some(Ok(())) => eprintln!("[shot] {path}"),
        Some(Err(e)) => eprintln!("[shot] {path}: {e}"),
        None => eprintln!("[shot] {path}: encode failed"),
    }
}
