/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(unsafe_code)]

use euclid::default::Size2D;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use surfman::Adapter;
use surfman::Connection;
use surfman::Context;
use surfman::ContextAttributes;
use surfman::Device;
use surfman::Error;
use surfman::NativeContext;
use surfman::NativeDevice;
use surfman::NativeWidget;
use surfman::Surface;
use surfman::SurfaceAccess;
use surfman::SurfaceInfo;
use surfman::SurfaceTexture;
use surfman::SurfaceType;
use surfman_chains::SurfmanProvider;
use surfman_chains::SwapChain;

use webrender_api::units::TexelRect;

/// This trait is used as a bridge between the different GL clients
/// in Servo that handles WebRender ExternalImages and the WebRender
/// ExternalImageHandler API.
//
/// This trait is used to notify lock/unlock messages and get the
/// required info that WR needs.
pub trait WebrenderExternalImageApi {
    fn lock(&mut self, id: u64) -> (u32, Size2D<i32>);
    fn unlock(&mut self, id: u64);
}

/// Type of Webrender External Image Handler.
pub enum WebrenderImageHandlerType {
    WebGL,
    Media,
}

/// List of Webrender external images to be shared among all external image
/// consumers (WebGL, Media).
/// It ensures that external image identifiers are unique.
pub struct WebrenderExternalImageRegistry {
    /// Map of all generated external images.
    external_images: HashMap<webrender_api::ExternalImageId, WebrenderImageHandlerType>,
    /// Id generator for the next external image identifier.
    next_image_id: u64,
}

impl WebrenderExternalImageRegistry {
    pub fn new() -> Self {
        Self {
            external_images: HashMap::new(),
            next_image_id: 0,
        }
    }

    pub fn next_id(
        &mut self,
        handler_type: WebrenderImageHandlerType,
    ) -> webrender_api::ExternalImageId {
        self.next_image_id += 1;
        let key = webrender_api::ExternalImageId(self.next_image_id);
        self.external_images.insert(key, handler_type);
        key
    }

    pub fn remove(&mut self, key: &webrender_api::ExternalImageId) {
        self.external_images.remove(key);
    }

    pub fn get(&self, key: &webrender_api::ExternalImageId) -> Option<&WebrenderImageHandlerType> {
        self.external_images.get(key)
    }
}

/// WebRender External Image Handler implementation.
pub struct WebrenderExternalImageHandlers {
    /// WebGL handler.
    webgl_handler: Option<Box<dyn WebrenderExternalImageApi>>,
    /// Media player handler.
    media_handler: Option<Box<dyn WebrenderExternalImageApi>>,
    /// Webrender external images.
    external_images: Arc<Mutex<WebrenderExternalImageRegistry>>,
}

impl WebrenderExternalImageHandlers {
    pub fn new() -> (Self, Arc<Mutex<WebrenderExternalImageRegistry>>) {
        let external_images = Arc::new(Mutex::new(WebrenderExternalImageRegistry::new()));
        (
            Self {
                webgl_handler: None,
                media_handler: None,
                external_images: external_images.clone(),
            },
            external_images,
        )
    }

    pub fn set_handler(
        &mut self,
        handler: Box<dyn WebrenderExternalImageApi>,
        handler_type: WebrenderImageHandlerType,
    ) {
        match handler_type {
            WebrenderImageHandlerType::WebGL => self.webgl_handler = Some(handler),
            WebrenderImageHandlerType::Media => self.media_handler = Some(handler),
        }
    }
}

impl webrender_api::ExternalImageHandler for WebrenderExternalImageHandlers {
    /// Lock the external image. Then, WR could start to read the
    /// image content.
    /// The WR client should not change the image content until the
    /// unlock() call.
    fn lock(
        &mut self,
        key: webrender_api::ExternalImageId,
        _channel_index: u8,
        _rendering: webrender_api::ImageRendering,
    ) -> webrender_api::ExternalImage {
        let external_images = self.external_images.lock().unwrap();
        let handler_type = external_images
            .get(&key)
            .expect("Tried to get unknown external image");
        let (texture_id, uv) = match handler_type {
            WebrenderImageHandlerType::WebGL => {
                let (texture_id, size) = self.webgl_handler.as_mut().unwrap().lock(key.0);
                (
                    texture_id,
                    TexelRect::new(0.0, size.height as f32, size.width as f32, 0.0),
                )
            },
            WebrenderImageHandlerType::Media => {
                let (texture_id, size) = self.media_handler.as_mut().unwrap().lock(key.0);
                (
                    texture_id,
                    TexelRect::new(0.0, 0.0, size.width as f32, size.height as f32),
                )
            },
        };
        webrender_api::ExternalImage {
            uv,
            source: webrender_api::ExternalImageSource::NativeTexture(texture_id),
        }
    }

    /// Unlock the external image. The WR should not read the image
    /// content after this call.
    fn unlock(&mut self, key: webrender_api::ExternalImageId, _channel_index: u8) {
        let external_images = self.external_images.lock().unwrap();
        let handler_type = external_images
            .get(&key)
            .expect("Tried to get unknown external image");
        match handler_type {
            WebrenderImageHandlerType::WebGL => self.webgl_handler.as_mut().unwrap().unlock(key.0),
            WebrenderImageHandlerType::Media => self.media_handler.as_mut().unwrap().unlock(key.0),
        };
    }
}

/// A bridge between webrender and surfman
// TODO: move this into a different crate so that script doesn't depend on surfman
#[derive(Clone)]
pub struct WebrenderSurfman(Rc<WebrenderSurfmanData>);

struct WebrenderSurfmanData {
    device: RefCell<Device>,
    context: RefCell<Context>,
    // We either render to a swap buffer or to a native widget
    swap_chain: Option<SwapChain<Device>>,
}

impl Drop for WebrenderSurfmanData {
    fn drop(&mut self) {
        let ref mut device = self.device.borrow_mut();
        let ref mut context = self.context.borrow_mut();
        if let Some(ref swap_chain) = self.swap_chain {
            let _ = swap_chain.destroy(device, context);
        }
        let _ = device.destroy_context(context);
    }
}

impl WebrenderSurfman {
    pub fn create(
        connection: &Connection,
        adapter: &Adapter,
        context_attributes: ContextAttributes,
        surface_type: SurfaceType<NativeWidget>,
    ) -> Result<Self, Error> {
        let mut device = connection.create_device(&adapter)?;
        let context_descriptor = device.create_context_descriptor(&context_attributes)?;
        let mut context = device.create_context(&context_descriptor)?;
        let surface_access = SurfaceAccess::GPUOnly;
        let headless = match surface_type {
            SurfaceType::Widget { .. } => false,
            SurfaceType::Generic { .. } => true,
        };
        let surface = device.create_surface(&context, surface_access, surface_type)?;
        device
            .bind_surface_to_context(&mut context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(&mut context, &mut surface);
                err
            })?;
        let swap_chain = if headless {
            let surface_provider = Box::new(SurfmanProvider::new(surface_access));
            Some(SwapChain::create_attached(
                &mut device,
                &mut context,
                surface_provider,
            )?)
        } else {
            None
        };
        let device = RefCell::new(device);
        let context = RefCell::new(context);
        let data = WebrenderSurfmanData {
            device,
            context,
            swap_chain,
        };
        Ok(WebrenderSurfman(Rc::new(data)))
    }

    pub fn create_surface_texture(
        &self,
        surface: Surface,
    ) -> Result<SurfaceTexture, (Error, Surface)> {
        let ref device = self.0.device.borrow();
        let ref mut context = self.0.context.borrow_mut();
        device.create_surface_texture(context, surface)
    }

    pub fn destroy_surface_texture(
        &self,
        surface_texture: SurfaceTexture,
    ) -> Result<Surface, (Error, SurfaceTexture)> {
        let ref device = self.0.device.borrow();
        let ref mut context = self.0.context.borrow_mut();
        device.destroy_surface_texture(context, surface_texture)
    }

    pub fn make_gl_context_current(&self) -> Result<(), Error> {
        let ref device = self.0.device.borrow();
        let ref context = self.0.context.borrow();
        device.make_context_current(context)
    }

    pub fn swap_chain(&self) -> Result<&SwapChain<Device>, Error> {
        self.0.swap_chain.as_ref().ok_or(Error::WidgetAttached)
    }

    pub fn resize(&self, size: Size2D<i32>) -> Result<(), Error> {
        let ref mut device = self.0.device.borrow_mut();
        let ref mut context = self.0.context.borrow_mut();
        self.swap_chain()?.resize(device, context, size)
    }

    pub fn present(&self) -> Result<(), Error> {
        let ref mut device = self.0.device.borrow_mut();
        let ref mut context = self.0.context.borrow_mut();
        if let Some(ref swap_chain) = self.0.swap_chain {
            return swap_chain.swap_buffers(device, context);
        }
        let mut surface = device.unbind_surface_from_context(context)?.unwrap();
        device.present_surface(context, &mut surface)?;
        device
            .bind_surface_to_context(context, surface)
            .map_err(|(err, mut surface)| {
                let _ = device.destroy_surface(context, &mut surface);
                err
            })
    }

    pub fn connection(&self) -> Connection {
        let ref device = self.0.device.borrow();
        device.connection()
    }

    pub fn adapter(&self) -> Adapter {
        let ref device = self.0.device.borrow();
        device.adapter()
    }

    pub fn native_context(&self) -> NativeContext {
        let ref device = self.0.device.borrow();
        let ref context = self.0.context.borrow();
        device.native_context(context)
    }

    pub fn native_device(&self) -> NativeDevice {
        let ref device = self.0.device.borrow();
        device.native_device()
    }

    pub fn context_attributes(&self) -> ContextAttributes {
        let ref device = self.0.device.borrow();
        let ref context = self.0.context.borrow();
        let ref descriptor = device.context_descriptor(context);
        device.context_descriptor_attributes(descriptor)
    }

    pub fn context_surface_info(&self) -> Result<Option<SurfaceInfo>, Error> {
        let ref device = self.0.device.borrow();
        let ref context = self.0.context.borrow();
        device.context_surface_info(context)
    }

    pub fn surface_info(&self, surface: &Surface) -> SurfaceInfo {
        let ref device = self.0.device.borrow();
        device.surface_info(surface)
    }

    pub fn surface_texture_object(&self, surface: &SurfaceTexture) -> u32 {
        let ref device = self.0.device.borrow();
        device.surface_texture_object(surface)
    }

    pub fn get_proc_address(&self, name: &str) -> *const c_void {
        let ref device = self.0.device.borrow();
        let ref context = self.0.context.borrow();
        device.get_proc_address(context, name)
    }
}
