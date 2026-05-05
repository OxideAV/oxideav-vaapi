//! Safe wrapper around `VAContextID` + the surfaces it renders into.
//!
//! A VA-API decode context is created from:
//!
//! * a [`Config`](crate::config::Config) (profile + entrypoint + attrs),
//! * a coded `(width, height)` — for H.264 this must be MB-aligned,
//!   so 1080-line content is allocated as 1088 lines internally,
//! * a set of `VASurfaceID` "render targets" — the GPU writes
//!   decoded pictures into these.
//!
//! [`Context::new`] performs the two-step setup:
//!
//! 1. `vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, w, h, …)` allocates
//!    the surfaces.
//! 2. `vaCreateContext(dpy, config.id(), w, h, 0, surfaces, …)` builds
//!    the context handle.
//!
//! `Drop` tears down in reverse: `vaDestroyContext`, then
//! `vaDestroySurfaces`.

use std::marker::PhantomData;

use crate::config::Config;
use crate::display::{Display, VaError};
use crate::sys::{
    self, error_str, VAContextID, VADisplay, VASurfaceID, VA_RT_FORMAT_YUV420,
    VA_STATUS_SUCCESS,
};

/// Owned `VAContextID` plus the render-target surfaces attached to it.
pub struct Context<'d> {
    dpy: VADisplay,
    surfaces: Vec<VASurfaceID>,
    context_id: VAContextID,
    width: u32,
    height: u32,
    _marker: PhantomData<&'d Display>,
}

impl<'d> Context<'d> {
    /// Allocate `num_surfaces` `VA_RT_FORMAT_YUV420` surfaces sized
    /// `width x height` and bind them to a new context built from
    /// `config`.
    pub fn new(
        dpy: &'d Display,
        config: &Config<'d>,
        width: u32,
        height: u32,
        num_surfaces: u32,
    ) -> Result<Self, VaError> {
        if num_surfaces == 0 {
            return Err(VaError::Va {
                status: 0,
                message: "Context::new: num_surfaces must be >= 1".into(),
            });
        }

        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;

        let mut surfaces: Vec<VASurfaceID> = vec![0; num_surfaces as usize];
        // SAFETY: `dpy.raw()` valid; `surfaces` is large enough for
        // `num_surfaces` writes; attribute list is NULL/0 so libva
        // picks defaults (NV12 internal layout on nvidia-vaapi-driver).
        let status = unsafe {
            (vt.va_create_surfaces)(
                dpy.raw(),
                VA_RT_FORMAT_YUV420,
                width,
                height,
                surfaces.as_mut_ptr(),
                num_surfaces,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != VA_STATUS_SUCCESS {
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        let mut context_id: VAContextID = 0;
        // SAFETY: `surfaces` is now populated by libva. `flag = 0` is
        // the default — see header comment in va.h on vaCreateContext.
        let status = unsafe {
            (vt.va_create_context)(
                dpy.raw(),
                config.id(),
                width as i32,
                height as i32,
                0,
                surfaces.as_mut_ptr(),
                surfaces.len() as i32,
                &mut context_id,
            )
        };
        if status != VA_STATUS_SUCCESS {
            // Best-effort: tear down the surfaces we just allocated.
            // SAFETY: `surfaces` is the list we just got from libva.
            unsafe {
                let _ = (vt.va_destroy_surfaces)(
                    dpy.raw(),
                    surfaces.as_mut_ptr(),
                    surfaces.len() as i32,
                );
            }
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        Ok(Self {
            dpy: dpy.raw(),
            surfaces,
            context_id,
            width,
            height,
            _marker: PhantomData,
        })
    }

    /// Raw `VAContextID`. Valid until this `Context` is dropped.
    pub fn id(&self) -> VAContextID {
        self.context_id
    }

    /// Render-target surfaces, in allocation order.
    pub fn surfaces(&self) -> &[VASurfaceID] {
        &self.surfaces
    }

    /// Width/height the context was created with (matches the surface
    /// allocation).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl<'d> Drop for Context<'d> {
    fn drop(&mut self) {
        if let Ok(vt) = sys::vtable() {
            // SAFETY: context_id came from a successful vaCreateContext
            // and has not been destroyed yet. Best-effort — ignore
            // VAStatus because there's nothing actionable at drop time.
            unsafe {
                let _ = (vt.va_destroy_context)(self.dpy, self.context_id);
                let _ = (vt.va_destroy_surfaces)(
                    self.dpy,
                    self.surfaces.as_mut_ptr(),
                    self.surfaces.len() as i32,
                );
            }
        }
    }
}
