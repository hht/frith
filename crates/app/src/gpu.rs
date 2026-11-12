//! Skia on Metal for the root window: the Metal device, its command queue, the Skia context,
//! and the `CAMetalLayer` that backs the window's content view.
//!
//! Answers: how does a Skia frame reach the window?
//! Must not know: what is drawn or why.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLPixelFormat,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin, backend_render_targets, mtl};
use skia_safe::{Canvas, ColorType, EncodedImageFormat};
use winit::raw_window_handle::RawWindowHandle;
use winit::window::Window;

pub struct Gpu {
    // Declared first so it drops before the device and queue it was created from.
    pub ctx: DirectContext,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
    _device: Retained<ProtocolObject<dyn MTLDevice>>,
}

impl Gpu {
    /// Backs `window`'s content view with a Metal layer that Skia draws into.
    pub fn attach(window: &dyn Window) -> Gpu {
        let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
        let queue = device.newCommandQueue().expect("Metal command queue");
        // SAFETY: both handles are live Metal objects that `Gpu` keeps alive until after
        // `ctx` has dropped (see field order).
        let backend = unsafe {
            mtl::BackendContext::new(Retained::as_ptr(&device) as mtl::Handle, Retained::as_ptr(&queue) as mtl::Handle)
        };
        let ctx = gpu::direct_contexts::make_metal(&backend, None).expect("Skia Metal context");

        let size = window.surface_size();
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setPresentsWithTransaction(false);
        // Screenshots read the drawable back, which a framebuffer-only texture forbids.
        layer.setFramebufferOnly(false);
        layer.setDrawableSize(CGSize::new(size.width.max(1) as f64, size.height.max(1) as f64));
        let handle = window.rwh_06_window_handle().window_handle().expect("window handle");
        let RawWindowHandle::AppKit(h) = handle.as_raw() else { panic!("macOS only") };
        // SAFETY: winit guarantees `ns_view` is a valid NSView for the window's lifetime,
        // and this runs on the main thread.
        let view = unsafe { &*(h.ns_view.as_ptr() as *const NSView) };
        view.setWantsLayer(true);
        view.setLayer(Some(&layer.clone().into_super()));
        Gpu { ctx, queue, layer, _device: device }
    }

    pub fn resize(&self, width: u32, height: u32) {
        self.layer.setDrawableSize(CGSize::new(width.max(1) as f64, height.max(1) as f64));
    }

    /// Draws one frame with `draw` and presents it; with `shot`, also writes the frame to
    /// that path as PNG. Returns `None` when no drawable is available (window hidden).
    pub fn present<R>(&mut self, shot: Option<&str>, draw: impl FnOnce(&Canvas, &mut DirectContext) -> R) -> Option<R> {
        let drawable = self.layer.nextDrawable()?;
        let size = self.layer.drawableSize();
        // SAFETY: the texture belongs to `drawable`, which outlives `surface` below.
        let tex = unsafe { mtl::TextureInfo::new(Retained::as_ptr(&drawable.texture()) as mtl::Handle) };
        let target = backend_render_targets::make_mtl((size.width as i32, size.height as i32), &tex);
        let mut surface = gpu::surfaces::wrap_backend_render_target(
            &mut self.ctx,
            &target,
            SurfaceOrigin::TopLeft,
            ColorType::BGRA8888,
            None,
            None,
        )
        .expect("wrap Metal drawable");
        let out = draw(surface.canvas(), &mut self.ctx);
        if let Some(path) = shot {
            save_png(&mut surface, &mut self.ctx, path);
        }
        if let Err(e) = self.ctx.flush_and_submit() {
            eprintln!("frith: GPU submit failed: {e:?}");
        }
        drop(surface);
        let cb = self.queue.commandBuffer().expect("Metal command buffer");
        let d: Retained<ProtocolObject<dyn MTLDrawable>> = (&drawable).into();
        cb.presentDrawable(&d);
        cb.commit();
        Some(out)
    }
}

fn save_png(surface: &mut skia_safe::Surface, ctx: &mut DirectContext, path: &str) {
    let image = surface.image_snapshot();
    let png = image
        .make_raster_image(Some(&mut *ctx), None)
        .and_then(|raster| raster.encode(Some(&mut *ctx), EncodedImageFormat::PNG, None));
    match png.map(|png| std::fs::write(path, png.as_bytes())) {
        Some(Ok(())) => eprintln!("frith: screenshot {path}"),
        Some(Err(e)) => eprintln!("frith: screenshot {path}: {e}"),
        None => eprintln!("frith: screenshot {path}: encode failed"),
    }
}
