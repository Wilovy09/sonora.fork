//! The Linux decoder: a GStreamer pipeline that demuxes and decodes with whatever H.264 decoder
//! the system ranks best (VA-API hardware when present, software otherwise) and converts to
//! NV12, copied out into tightly packed planes.
//!
//! Nothing links GStreamer. Like webkit2gtk in `webview`, the libraries are opened at runtime with
//! `dlopen`, so a system without them, or without an H.264 decoder, simply answers
//! `supported() == false` and keeps the static cover. The few structs read here, `GstMapInfo` and
//! the head of `GstVideoMeta`, are laid out as in `gstreamer-sys`; everything else goes through
//! functions, frame size and rate included, so no other struct layout is relied on.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures::SinkExt as _;
use futures::channel::mpsc;

use crate::{Frame, Picture};

type Ptr = *mut c_void;
type Bool = c_int;

/// The libraries the pipeline needs. `dlsym` walks each handle's dependencies, which is how glib
/// and gobject are reached without opening them by name.
const LIBRARIES: [&str; 3] = [
    "libgstreamer-1.0.so.0\0",
    "libgstapp-1.0.so.0\0",
    "libgstvideo-1.0.so.0\0",
];

/// Demux, decode with the best decoder the system ranks, and hand NV12 to the app. The sink is
/// not synced to a clock: the bounded queue the frames go into sets the pace.
const PIPELINE: &CStr = c"filesrc name=source ! decodebin ! videoconvert \
    ! video/x-raw,format=NV12 ! appsink name=sink sync=false max-buffers=2";

/// The elements the pipeline names, which have to be installed for it to build.
const ELEMENTS: [&CStr; 5] = [
    c"filesrc",
    c"qtdemux",
    c"decodebin",
    c"videoconvert",
    c"appsink",
];

/// How long opening may take before the file counts as unreadable, in nanoseconds.
const PREROLL: u64 = 5_000_000_000;

/// The frame rate assumed when the stream does not say, which only paces the frames.
const FALLBACK_RATE: (c_int, c_int) = (30, 1);

// The constants below are GStreamer's, as `gstreamer-sys` spells them.
const GST_STATE_NULL: c_int = 1;
const GST_STATE_PAUSED: c_int = 3;
const GST_STATE_PLAYING: c_int = 4;
const GST_STATE_CHANGE_SUCCESS: c_int = 1;
const GST_STATE_CHANGE_NO_PREROLL: c_int = 3;
const GST_FORMAT_TIME: c_int = 3;
const GST_SEEK_FLAG_FLUSH: c_uint = 1;
const GST_SEEK_FLAG_KEY_UNIT: c_uint = 4;
const GST_MAP_READ: c_uint = 1;
const GST_PAD_SINK: c_int = 2;
const GST_RANK_MARGINAL: c_int = 64;
const GST_ELEMENT_FACTORY_TYPE_DECODER: u64 = 1;
const GST_ELEMENT_FACTORY_TYPE_MEDIA_VIDEO: u64 = 1 << 49;

/// `GstMapInfo`, filled by `gst_buffer_map`.
#[repr(C)]
struct MapInfo {
    memory: Ptr,
    flags: c_uint,
    data: *mut u8,
    size: usize,
    maxsize: usize,
    user_data: [Ptr; 4],
    reserved: [Ptr; 4],
}

/// The head of `GstVideoMeta`, as far as the plane offsets and strides. Only ever read through a
/// pointer GStreamer hands out, so the fields after these do not need to be declared.
#[repr(C)]
struct VideoMeta {
    meta_flags: c_uint,
    meta_info: *const c_void,
    buffer: Ptr,
    flags: c_uint,
    format: c_int,
    id: c_int,
    width: c_uint,
    height: c_uint,
    n_planes: c_uint,
    offset: [usize; 4],
    stride: [c_int; 4],
}

/// A `GError`, read only for its message.
#[repr(C)]
struct Fault {
    _domain: u32,
    _code: c_int,
    message: *const c_char,
}

macro_rules! symbols {
    ($($name:ident: unsafe extern "C" fn($($arg:ty),* $(,)?) $(-> $ret:ty)?,)*) => {
        /// Every call this backend makes, resolved once out of the GStreamer handles.
        struct Api {
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Api {
            /// Resolves the whole table or nothing: half a table would fail later and further
            /// from the cause.
            unsafe fn load(handles: &[Ptr]) -> Option<Self> {
                Some(Self {
                    $($name: unsafe { symbol(handles, concat!(stringify!($name), "\0"))? },)*
                })
            }
        }
    };
}

symbols! {
    gst_init_check: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char, *mut *mut Fault) -> Bool,
    gst_parse_launch: unsafe extern "C" fn(*const c_char, *mut *mut Fault) -> Ptr,
    gst_bin_get_by_name: unsafe extern "C" fn(Ptr, *const c_char) -> Ptr,
    gst_util_set_object_arg: unsafe extern "C" fn(Ptr, *const c_char, *const c_char),
    gst_element_set_state: unsafe extern "C" fn(Ptr, c_int) -> c_int,
    gst_element_get_state: unsafe extern "C" fn(Ptr, *mut c_int, *mut c_int, u64) -> c_int,
    gst_element_seek_simple: unsafe extern "C" fn(Ptr, c_int, c_uint, i64) -> Bool,
    gst_object_unref: unsafe extern "C" fn(Ptr),
    gst_element_factory_find: unsafe extern "C" fn(*const c_char) -> Ptr,
    gst_element_factory_list_get_elements: unsafe extern "C" fn(u64, c_int) -> Ptr,
    gst_element_factory_list_filter: unsafe extern "C" fn(Ptr, Ptr, c_int, Bool) -> Ptr,
    gst_plugin_feature_list_free: unsafe extern "C" fn(Ptr),
    gst_caps_from_string: unsafe extern "C" fn(*const c_char) -> Ptr,
    gst_mini_object_unref: unsafe extern "C" fn(Ptr),
    gst_sample_get_buffer: unsafe extern "C" fn(Ptr) -> Ptr,
    gst_sample_get_caps: unsafe extern "C" fn(Ptr) -> Ptr,
    gst_caps_get_structure: unsafe extern "C" fn(Ptr, c_uint) -> Ptr,
    gst_structure_get_int: unsafe extern "C" fn(Ptr, *const c_char, *mut c_int) -> Bool,
    gst_structure_get_fraction: unsafe extern "C" fn(Ptr, *const c_char, *mut c_int, *mut c_int) -> Bool,
    gst_structure_get_string: unsafe extern "C" fn(Ptr, *const c_char) -> *const c_char,
    gst_buffer_map: unsafe extern "C" fn(Ptr, *mut MapInfo, c_uint) -> Bool,
    gst_buffer_unmap: unsafe extern "C" fn(Ptr, *mut MapInfo),
    gst_buffer_get_meta: unsafe extern "C" fn(Ptr, usize) -> *const VideoMeta,
    gst_video_meta_api_get_type: unsafe extern "C" fn() -> usize,
    gst_app_sink_pull_sample: unsafe extern "C" fn(Ptr) -> Ptr,
    gst_app_sink_is_eos: unsafe extern "C" fn(Ptr) -> Bool,
    g_error_free: unsafe extern "C" fn(*mut Fault),
}

/// Whether GStreamer is installed with everything the pipeline needs, an H.264 decoder included.
/// Checked once, since the first call loads the libraries and scans the plugin registry.
pub(crate) fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Some(api) = api() else {
            log::info!("motion: gstreamer is not installed");
            return false;
        };
        if let Some(missing) = ELEMENTS.into_iter().find(|name| !api.has_element(name)) {
            log::info!("motion: gstreamer has no {}", missing.to_string_lossy());
            return false;
        }
        let found = api.has_h264_decoder();
        if !found {
            log::info!("motion: gstreamer has no h.264 decoder");
        }
        found
    })
}

/// Opens `path`, prerolls it so a file the system cannot decode fails here, then decodes it
/// forever on a thread of its own. The GPU draws the frame at any size, so `edge` is not needed.
pub(crate) fn spawn(path: &Path, _edge: u32, sender: mpsc::Sender<Frame>) -> Result<()> {
    let api = api().context("gstreamer is not installed")?;
    let path = path.to_str().context("the motion file path is not utf-8")?;
    let location = CString::new(path).context("the motion file path holds a nul")?;
    let pipeline = Pipeline::open(api, &location)?;
    let shown = path.to_owned();
    std::thread::Builder::new()
        .name("motion-decoder".into())
        .spawn(move || {
            if let Err(error) = decode(&pipeline, sender) {
                log::warn!("motion: cannot decode {shown}: {error:#}");
            }
        })
        .context("cannot start the motion decoder thread")?;
    Ok(())
}

/// A playing pipeline and its sink, stopped and released when dropped.
struct Pipeline {
    api: &'static Api,
    pipeline: Ptr,
    sink: Ptr,
}

// SAFETY: GStreamer objects are reference counted and thread safe; the pipeline is only driven
// from the decoder thread once it is handed over.
unsafe impl Send for Pipeline {}

impl Pipeline {
    fn open(api: &'static Api, location: &CStr) -> Result<Self> {
        let mut fault = std::ptr::null_mut();
        let pipeline = unsafe { (api.gst_parse_launch)(PIPELINE.as_ptr(), &mut fault) };
        if pipeline.is_null() {
            bail!("cannot build the motion pipeline: {}", api.take(fault));
        }
        // A pipeline built with a recoverable error still comes back; the error only names a
        // missing piece, which preroll reports again if it matters.
        api.take(fault);
        let mut this = Self {
            api,
            pipeline,
            sink: std::ptr::null_mut(),
        };
        let source = unsafe { (api.gst_bin_get_by_name)(pipeline, c"source".as_ptr()) };
        if source.is_null() {
            bail!("the motion pipeline has no source");
        }
        unsafe {
            (api.gst_util_set_object_arg)(source, c"location".as_ptr(), location.as_ptr());
            (api.gst_object_unref)(source);
        }
        this.sink = unsafe { (api.gst_bin_get_by_name)(pipeline, c"sink".as_ptr()) };
        if this.sink.is_null() {
            bail!("the motion pipeline has no sink");
        }

        unsafe { (api.gst_element_set_state)(pipeline, GST_STATE_PAUSED) };
        let mut state = 0;
        let mut pending = 0;
        let prerolled =
            unsafe { (api.gst_element_get_state)(pipeline, &mut state, &mut pending, PREROLL) };
        if prerolled != GST_STATE_CHANGE_SUCCESS && prerolled != GST_STATE_CHANGE_NO_PREROLL {
            bail!("gstreamer cannot decode the file");
        }
        unsafe { (api.gst_element_set_state)(pipeline, GST_STATE_PLAYING) };
        Ok(this)
    }

    /// Seeks back to the first frame.
    fn rewind(&self) -> Result<()> {
        let flags = GST_SEEK_FLAG_FLUSH | GST_SEEK_FLAG_KEY_UNIT;
        let seeked =
            unsafe { (self.api.gst_element_seek_simple)(self.pipeline, GST_FORMAT_TIME, flags, 0) };
        if seeked == 0 {
            bail!("cannot go back to the start of the file");
        }
        Ok(())
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        unsafe {
            (self.api.gst_element_set_state)(self.pipeline, GST_STATE_NULL);
            if !self.sink.is_null() {
                (self.api.gst_object_unref)(self.sink);
            }
            (self.api.gst_object_unref)(self.pipeline);
        }
    }
}

/// Pulls frames until the receiver goes away, seeking back to the start at the end of the file.
/// Timestamps come from the frame count and the stream's rate, which restarts with the file.
fn decode(pipeline: &Pipeline, mut sender: mpsc::Sender<Frame>) -> Result<()> {
    let api = pipeline.api;
    let mut index = 0u64;
    loop {
        let sample = unsafe { (api.gst_app_sink_pull_sample)(pipeline.sink) };
        if sample.is_null() {
            if unsafe { (api.gst_app_sink_is_eos)(pipeline.sink) } == 0 {
                bail!("the motion pipeline stopped");
            }
            if index == 0 {
                bail!("the file has no video frames");
            }
            pipeline.rewind()?;
            index = 0;
            continue;
        }
        let copied = unsafe { copy(api, sample) };
        unsafe { (api.gst_mini_object_unref)(sample) };
        let (picture, (numerator, denominator)) = copied?;
        let pts = Duration::from_secs_f64(index as f64 * denominator as f64 / numerator as f64);
        if futures::executor::block_on(sender.send(Frame { picture, pts })).is_err() {
            return Ok(());
        }
        index += 1;
    }
}

/// Copies an NV12 sample into tightly packed planes, along with the stream's frame rate.
///
/// # Safety
///
/// `sample` must be a live `GstSample` from the pipeline's sink.
unsafe fn copy(api: &Api, sample: Ptr) -> Result<(Picture, (c_int, c_int))> {
    let caps = unsafe { (api.gst_sample_get_caps)(sample) };
    let buffer = unsafe { (api.gst_sample_get_buffer)(sample) };
    if caps.is_null() || buffer.is_null() {
        bail!("a decoded frame has no format or buffer");
    }
    let structure = unsafe { (api.gst_caps_get_structure)(caps, 0) };
    if structure.is_null() {
        bail!("a decoded frame has no format");
    }
    let (mut width, mut height) = (0, 0);
    let sized = unsafe {
        (api.gst_structure_get_int)(structure, c"width".as_ptr(), &mut width) != 0
            && (api.gst_structure_get_int)(structure, c"height".as_ptr(), &mut height) != 0
    };
    if !sized || width <= 0 || height <= 0 {
        bail!("a decoded frame has no size");
    }
    let (mut numerator, mut denominator) = (0, 0);
    let rated = unsafe {
        (api.gst_structure_get_fraction)(
            structure,
            c"framerate".as_ptr(),
            &mut numerator,
            &mut denominator,
        ) != 0
    };
    let rate = match rated && numerator > 0 && denominator > 0 {
        true => (numerator, denominator),
        false => FALLBACK_RATE,
    };
    let colorimetry = unsafe { (api.gst_structure_get_string)(structure, c"colorimetry".as_ptr()) };
    let video_range = match colorimetry.is_null() {
        true => true,
        false => video_range(&unsafe { CStr::from_ptr(colorimetry) }.to_string_lossy()),
    };

    let (width, height) = (width as usize, height as usize);
    // A decoder that pads its frames says how in a video meta; a plain buffer has GStreamer's
    // default NV12 layout, with rows rounded up to four bytes.
    let meta = unsafe { (api.gst_buffer_get_meta)(buffer, (api.gst_video_meta_api_get_type)()) };
    let (offsets, strides) = match unsafe { meta.as_ref() } {
        Some(meta) if meta.n_planes >= 2 => (
            [meta.offset[0], meta.offset[1]],
            [meta.stride[0] as isize, meta.stride[1] as isize],
        ),
        _ => {
            let stride = width.next_multiple_of(4);
            (
                [0, stride * height.next_multiple_of(2)],
                [stride as isize; 2],
            )
        }
    };

    let mut map = MapInfo {
        memory: std::ptr::null_mut(),
        flags: 0,
        data: std::ptr::null_mut(),
        size: 0,
        maxsize: 0,
        user_data: [std::ptr::null_mut(); 4],
        reserved: [std::ptr::null_mut(); 4],
    };
    if unsafe { (api.gst_buffer_map)(buffer, &mut map, GST_MAP_READ) } == 0 {
        bail!("cannot map a decoded frame");
    }
    let bytes = match map.data.is_null() {
        true => &[][..],
        // SAFETY: a mapped buffer's `size` bytes are readable until it is unmapped.
        false => unsafe { std::slice::from_raw_parts(map.data, map.size) },
    };
    let planes = (|| {
        let y = plane(bytes, offsets[0], strides[0], height, width)?;
        let cb_cr = plane(
            bytes,
            offsets[1],
            strides[1],
            height.div_ceil(2),
            width.div_ceil(2) * 2,
        )?;
        Ok::<_, anyhow::Error>((y, cb_cr))
    })();
    unsafe { (api.gst_buffer_unmap)(buffer, &mut map) };
    let (y, cb_cr) = planes?;
    let picture = Picture::Nv12 {
        width: width as u32,
        height: height as u32,
        video_range,
        y,
        cb_cr,
    };
    Ok((picture, rate))
}

/// Packs `rows` rows of `bytes` bytes each, `stride` apart from `offset`, out of a mapped frame.
fn plane(data: &[u8], offset: usize, stride: isize, rows: usize, bytes: usize) -> Result<Vec<u8>> {
    let Ok(stride) = usize::try_from(stride) else {
        bail!("a decoded frame is stored bottom up");
    };
    if stride < bytes || offset + stride * (rows - 1) + bytes > data.len() {
        bail!("a decoded frame is smaller than its format says");
    }
    let mut packed = Vec::with_capacity(rows * bytes);
    for line in 0..rows {
        let start = offset + line * stride;
        packed.extend_from_slice(&data[start..start + bytes]);
    }
    Ok(packed)
}

/// Whether a caps `colorimetry` string means the video range. The named ones are, except the
/// two full range ones; the numeric form leads with the range, where `1` is 0 to 255.
fn video_range(colorimetry: &str) -> bool {
    match colorimetry {
        "sRGB" | "jpeg" => false,
        numeric if numeric.contains(':') => !numeric.starts_with("1:"),
        _ => true,
    }
}

impl Api {
    fn has_element(&self, name: &CStr) -> bool {
        let factory = unsafe { (self.gst_element_factory_find)(name.as_ptr()) };
        if factory.is_null() {
            return false;
        }
        unsafe { (self.gst_object_unref)(factory) };
        true
    }

    /// Whether any installed video decoder, hardware or software, takes H.264.
    fn has_h264_decoder(&self) -> bool {
        unsafe {
            let caps = (self.gst_caps_from_string)(c"video/x-h264".as_ptr());
            if caps.is_null() {
                return false;
            }
            let decoders = (self.gst_element_factory_list_get_elements)(
                GST_ELEMENT_FACTORY_TYPE_DECODER | GST_ELEMENT_FACTORY_TYPE_MEDIA_VIDEO,
                GST_RANK_MARGINAL,
            );
            let fitting = (self.gst_element_factory_list_filter)(decoders, caps, GST_PAD_SINK, 0);
            let found = !fitting.is_null();
            (self.gst_plugin_feature_list_free)(fitting);
            (self.gst_plugin_feature_list_free)(decoders);
            (self.gst_mini_object_unref)(caps);
            found
        }
    }

    /// The message of an error GStreamer handed back, freeing it. An empty one when there is none.
    fn take(&self, fault: *mut Fault) -> String {
        if fault.is_null() {
            return String::new();
        }
        let message = unsafe { (*fault).message };
        let text = match message.is_null() {
            true => String::new(),
            false => unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned(),
        };
        unsafe { (self.g_error_free)(fault) };
        text
    }
}

/// The symbol table, loaded and initialised on the first call and kept for the life of the
/// process. Unloading GStreamer is not safe once glib has registered its types, so nothing ever
/// calls `dlclose`.
fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    let mut handles = Vec::with_capacity(LIBRARIES.len());
    for soname in LIBRARIES {
        let handle =
            unsafe { libc::dlopen(soname.as_ptr().cast(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            log::info!("motion: cannot open {}", soname.trim_end_matches('\0'));
            return None;
        }
        handles.push(handle);
    }
    let Some(api) = (unsafe { Api::load(&handles) }) else {
        log::warn!("motion: gstreamer is missing symbols the decoder needs");
        return None;
    };
    let mut fault = std::ptr::null_mut();
    let started =
        unsafe { (api.gst_init_check)(std::ptr::null_mut(), std::ptr::null_mut(), &mut fault) };
    if started == 0 {
        log::warn!("motion: cannot start gstreamer: {}", api.take(fault));
        return None;
    }
    Some(api)
}

/// Resolves one symbol out of the first handle that has it. `dlsym` also searches each handle's
/// dependencies, which is how glib and gobject are reached.
unsafe fn symbol<T>(handles: &[Ptr], name: &str) -> Option<T> {
    assert!(size_of::<T>() == size_of::<Ptr>());
    handles.iter().find_map(|handle| {
        let found = unsafe { libc::dlsym(*handle, name.as_ptr().cast()) };
        (!found.is_null()).then(|| unsafe { std::mem::transmute_copy(&found) })
    })
}
