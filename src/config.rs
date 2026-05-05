//! Safe wrapper around `VAConfigID`.
//!
//! A `VAConfig` is the (profile, entrypoint, attribute-list) tuple
//! that the driver compiles into a configuration handle. It's the
//! prerequisite for creating a [`Context`](crate::context::Context).
//!
//! # Lifecycle
//!
//! 1. [`Config::new`] picks defaults — `attrib_list = NULL`,
//!    `num_attribs = 0` — by default; the driver substitutes its own
//!    defaults for required attributes (e.g. `VA_RT_FORMAT_YUV420`
//!    for an H.264 VLD decode config). Callers may pass an explicit
//!    `&[VAConfigAttrib]` slice to override.
//! 2. The wrapper holds the `VAConfigID` for the life of the value.
//!    `Drop` calls `vaDestroyConfig`.
//! 3. [`Config::supported_attributes`] queries the
//!    profile/entrypoint pair *without* needing a handle — it's a
//!    static call into `vaGetConfigAttributes` for an
//!    application-supplied list of attribute types.
//!
//! # Drop ordering
//!
//! `Config` borrows `&Display` for its construction call but stores
//! the raw `VADisplay` pointer plus a `PhantomData<&Display>` to
//! tie the wrapper's lifetime to the display: dropping the display
//! before the config is unsound, and the borrow checker will
//! prevent it.

use std::marker::PhantomData;

use crate::display::{Display, VaError};
use crate::sys::{
    self, attrib, error_str, VAConfigAttrib, VAConfigID, VADisplay, VA_ATTRIB_NOT_SUPPORTED,
    VA_STATUS_SUCCESS,
};

/// Safe `VAConfigID` wrapper.
///
/// Tied by lifetime to a [`Display`]; dropping the display while a
/// `Config` still references it is rejected at compile time.
pub struct Config<'d> {
    dpy: VADisplay,
    id: VAConfigID,
    profile: i32,
    entrypoint: i32,
    _marker: PhantomData<&'d Display>,
}

impl<'d> Config<'d> {
    /// Build a new configuration from a (profile, entrypoint,
    /// attributes) tuple.
    ///
    /// `attribs` may be empty — VA-API will pick defaults and the
    /// nvidia-vaapi-driver picks `VA_RT_FORMAT_YUV420` for H.264
    /// VLD decode automatically when no override is given.
    pub fn new(
        dpy: &'d Display,
        profile: i32,
        entrypoint: i32,
        attribs: &[VAConfigAttrib],
    ) -> Result<Self, VaError> {
        let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;

        let (attrib_ptr, num) = if attribs.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            // Cast away `const` for the call — libva's C signature is
            // `VAConfigAttrib *attrib_list` even though for
            // vaCreateConfig the values are read-only.
            (attribs.as_ptr() as *mut VAConfigAttrib, attribs.len() as i32)
        };

        let mut id: VAConfigID = 0;
        // SAFETY: `dpy.raw()` is non-null and valid (from a successful
        // `Display::open_drm`). `attrib_ptr` is either NULL (with
        // `num = 0`) or points to `num` valid `VAConfigAttrib` values.
        let status = unsafe {
            (vt.va_create_config)(
                dpy.raw(),
                profile,
                entrypoint,
                attrib_ptr as *mut std::ffi::c_void,
                num,
                &mut id,
            )
        };
        if status != VA_STATUS_SUCCESS {
            return Err(VaError::Va {
                status,
                message: error_str(vt, status),
            });
        }

        Ok(Self {
            dpy: dpy.raw(),
            id,
            profile,
            entrypoint,
            _marker: PhantomData,
        })
    }

    /// Raw `VAConfigID`. Valid until this `Config` is dropped.
    pub fn id(&self) -> VAConfigID {
        self.id
    }

    /// The profile this config was built for.
    pub fn profile(&self) -> i32 {
        self.profile
    }

    /// The entrypoint this config was built for.
    pub fn entrypoint(&self) -> i32 {
        self.entrypoint
    }

    /// Query the driver's value for a single attribute on the
    /// `(profile, entrypoint)` pair this config was created for.
    ///
    /// Returns `Ok(Some(value))` if the driver advertises the
    /// attribute, `Ok(None)` if it returns `VA_ATTRIB_NOT_SUPPORTED`,
    /// or `Err` on any libva failure. Attribute types live in
    /// [`crate::sys::attrib`].
    pub fn get_attribute(&self, attrib_type: i32) -> Result<Option<u32>, VaError> {
        get_attribute(self.dpy, self.profile, self.entrypoint, attrib_type)
    }

    /// Query a fixed list of attribute types and return their values
    /// (a `None` element means the driver said `VA_ATTRIB_NOT_SUPPORTED`).
    ///
    /// VA-API has no "list everything supported" call — the API
    /// requires the caller to provide the types it cares about.
    /// `Config::supported_attributes` walks a vendor-portable
    /// shortlist (`RTFormat`, `DecSliceMode`, `MaxPictureWidth`,
    /// `MaxPictureHeight`, `RateControl`) and returns the populated
    /// values for each one the driver answered.
    pub fn supported_attributes(&self) -> Result<Vec<VAConfigAttrib>, VaError> {
        supported_attributes(self.dpy, self.profile, self.entrypoint)
    }
}

impl<'d> Drop for Config<'d> {
    fn drop(&mut self) {
        if let Ok(vt) = sys::vtable() {
            // SAFETY: id is the value returned from the matching
            // vaCreateConfig and has not been destroyed yet.
            unsafe {
                let _ = (vt.va_destroy_config)(self.dpy, self.id);
            }
        }
    }
}

/// Standalone helper: query attributes for a `(profile, entrypoint)`
/// pair without needing to construct a `Config` first. Useful for
/// capability probing prior to `Config::new`.
///
/// `dpy` is treated as an opaque handle — it is forwarded to libva
/// dispatch and never dereferenced by Rust code. The
/// `not_unsafe_ptr_arg_deref` clippy lint is allowed here because
/// the pointer is opaque to this crate and libva owns the lifetime.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn supported_attributes(
    dpy: VADisplay,
    profile: i32,
    entrypoint: i32,
) -> Result<Vec<VAConfigAttrib>, VaError> {
    let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;

    // The shortlist of attribute types we surface — every working
    // VA-API driver knows about RTFormat for any decode profile;
    // the rest may report VA_ATTRIB_NOT_SUPPORTED depending on
    // codec/entrypoint.
    let mut list: [VAConfigAttrib; 5] = [
        VAConfigAttrib { ty: attrib::VAConfigAttribRTFormat, value: 0 },
        VAConfigAttrib { ty: attrib::VAConfigAttribDecSliceMode, value: 0 },
        VAConfigAttrib { ty: attrib::VAConfigAttribMaxPictureWidth, value: 0 },
        VAConfigAttrib { ty: attrib::VAConfigAttribMaxPictureHeight, value: 0 },
        VAConfigAttrib { ty: attrib::VAConfigAttribRateControl, value: 0 },
    ];

    // SAFETY: list is valid for `len()` writes and outlives the call.
    let status = unsafe {
        (vt.va_get_config_attributes)(
            dpy,
            profile,
            entrypoint,
            list.as_mut_ptr(),
            list.len() as i32,
        )
    };
    if status != VA_STATUS_SUCCESS {
        return Err(VaError::Va {
            status,
            message: error_str(vt, status),
        });
    }

    Ok(list
        .into_iter()
        .filter(|a| a.value != VA_ATTRIB_NOT_SUPPORTED)
        .collect())
}

/// Standalone helper: read one attribute value for a
/// `(profile, entrypoint)` pair.
///
/// `dpy` is treated as an opaque handle — see
/// [`supported_attributes`] for the same caveat.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn get_attribute(
    dpy: VADisplay,
    profile: i32,
    entrypoint: i32,
    attrib_type: i32,
) -> Result<Option<u32>, VaError> {
    let vt = sys::vtable().map_err(|e| VaError::Sys(e.to_string()))?;
    let mut one = [VAConfigAttrib { ty: attrib_type, value: 0 }];
    // SAFETY: `one` is valid for one write.
    let status = unsafe {
        (vt.va_get_config_attributes)(dpy, profile, entrypoint, one.as_mut_ptr(), 1)
    };
    if status != VA_STATUS_SUCCESS {
        return Err(VaError::Va {
            status,
            message: error_str(vt, status),
        });
    }
    Ok(if one[0].value == VA_ATTRIB_NOT_SUPPORTED {
        None
    } else {
        Some(one[0].value)
    })
}
