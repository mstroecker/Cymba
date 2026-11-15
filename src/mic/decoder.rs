//! FFmpeg's own `aac` decoder for the microphone's frames, from the system's
//! libavcodec loaded at runtime: `docs/app/mic.md#the-decoder`.

// The calls into libavcodec, and the two structures' first fields, are FFI:
// each block says what it relies on.
#![allow(unsafe_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use libloading::Library;

use crate::aap::mic::AUDIO_SPECIFIC_CONFIG;

/// The oldest libavcodec major tried: FFmpeg 4.0's.
pub const OLDEST_MAJOR: u32 = 58;
/// The newest: FFmpeg 9's. A newer one is not tried until checked here, as
/// it may change what is read below.
pub const NEWEST_MAJOR: u32 = 63;

/// The padding FFmpeg wants after the data it reads, zeroed:
/// `AV_INPUT_BUFFER_PADDING_SIZE` in every major tried.
const PADDING: usize = 64;

/// `AVERROR(EAGAIN)`: no frame yet, or no more.
const AVERROR_EAGAIN: c_int = -11;

/// `AV_SAMPLE_FMT_*`, the formats taken: one channel, so planar or not reads
/// the same.
const SAMPLE_FMT_S16: c_int = 1;
const SAMPLE_FMT_FLT: c_int = 3;
const SAMPLE_FMT_S16P: c_int = 6;
const SAMPLE_FMT_FLTP: c_int = 8;

/// The first fields of `AVCodecParameters`, the same in every major tried.
#[repr(C)]
struct Parameters {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
}

/// The first fields of `AVFrame`, the same in every major tried.
#[repr(C)]
struct Frame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
}

/// The libavcodec functions used, from the library that loaded. The library
/// stays loaded for the rest of the run.
struct Api {
    version: unsafe extern "C" fn() -> c_uint,
    find_decoder_by_name: unsafe extern "C" fn(*const c_char) -> *const c_void,
    alloc_context3: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    free_context: unsafe extern "C" fn(*mut *mut c_void),
    parameters_alloc: unsafe extern "C" fn() -> *mut Parameters,
    parameters_free: unsafe extern "C" fn(*mut *mut Parameters),
    parameters_to_context: unsafe extern "C" fn(*mut c_void, *const Parameters) -> c_int,
    open2: unsafe extern "C" fn(*mut c_void, *const c_void, *mut *mut c_void) -> c_int,
    send_packet: unsafe extern "C" fn(*mut c_void, *const c_void) -> c_int,
    receive_frame: unsafe extern "C" fn(*mut c_void, *mut Frame) -> c_int,
    packet_alloc: unsafe extern "C" fn() -> *mut c_void,
    packet_free: unsafe extern "C" fn(*mut *mut c_void),
    packet_from_data: unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int,
    packet_unref: unsafe extern "C" fn(*mut c_void),
    // libavutil's, which libavcodec loads and dlsym finds through it.
    frame_alloc: unsafe extern "C" fn() -> *mut Frame,
    frame_free: unsafe extern "C" fn(*mut *mut Frame),
    frame_unref: unsafe extern "C" fn(*mut Frame),
    mallocz: unsafe extern "C" fn(usize) -> *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
}

/// Loads `name` and reads every function [`Api`] holds out of it.
fn load(name: &str) -> Result<Api> {
    // SAFETY: loading libavcodec runs its initializers, which have no
    // requirements of the caller.
    let library = unsafe { Library::new(name) }.with_context(|| format!("no {name}"))?;
    macro_rules! symbol {
        ($name:literal) => {
            // SAFETY: the type is the function's in every major tried.
            *unsafe { library.get(concat!($name, "\0").as_bytes()) }
                .with_context(|| format!("{name} has no {}", $name))?
        };
    }
    let api = Api {
        version: symbol!("avcodec_version"),
        find_decoder_by_name: symbol!("avcodec_find_decoder_by_name"),
        alloc_context3: symbol!("avcodec_alloc_context3"),
        free_context: symbol!("avcodec_free_context"),
        parameters_alloc: symbol!("avcodec_parameters_alloc"),
        parameters_free: symbol!("avcodec_parameters_free"),
        parameters_to_context: symbol!("avcodec_parameters_to_context"),
        open2: symbol!("avcodec_open2"),
        send_packet: symbol!("avcodec_send_packet"),
        receive_frame: symbol!("avcodec_receive_frame"),
        packet_alloc: symbol!("av_packet_alloc"),
        packet_free: symbol!("av_packet_free"),
        packet_from_data: symbol!("av_packet_from_data"),
        packet_unref: symbol!("av_packet_unref"),
        frame_alloc: symbol!("av_frame_alloc"),
        frame_free: symbol!("av_frame_free"),
        frame_unref: symbol!("av_frame_unref"),
        mallocz: symbol!("av_mallocz"),
        free: symbol!("av_free"),
    };
    // The functions point into the library: it is never unloaded.
    std::mem::forget(library);
    Ok(api)
}

/// The libavcodec loaded, or why none would: loaded once, on first use.
fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| {
        let mut tried = Vec::new();
        for major in (OLDEST_MAJOR..=NEWEST_MAJOR).rev() {
            let name = format!("libavcodec.so.{major}");
            match load(&name) {
                Ok(api) => {
                    // SAFETY: avcodec_version takes nothing.
                    let version = unsafe { (api.version)() };
                    if version >> 16 != major {
                        return Err(format!("{name} says it is libavcodec {}", version >> 16));
                    }
                    tracing::info!(
                        "FFmpeg's libavcodec {}.{}.{} loaded, for the microphone",
                        version >> 16,
                        (version >> 8) & 0xFF,
                        version & 0xFF
                    );
                    return Ok(api);
                }
                Err(e) => tried.push(format!("{e:#}")),
            }
        }
        Err(format!(
            "no FFmpeg libavcodec {OLDEST_MAJOR} to {NEWEST_MAJOR} found ({})",
            tried.join("; ")
        ))
    })
    .as_ref()
    .map_err(String::as_str)
}

/// Whether the microphone's frames can be decoded here: a libavcodec loads,
/// and its `aac` decoder opens with the microphone's AudioSpecificConfig.
/// The reason, when not.
pub fn available() -> Result<()> {
    Decoder::new().map(drop)
}

/// FFmpeg's `aac` decoder, opened for the microphone's frames. Not `Send`:
/// it stays on the thread that opened it.
pub struct Decoder {
    api: &'static Api,
    context: *mut c_void,
    packet: *mut c_void,
    frame: *mut Frame,
}

impl Decoder {
    /// Opens the decoder with the microphone's AudioSpecificConfig.
    pub fn new() -> Result<Self> {
        let api = api().map_err(|e| anyhow::anyhow!("{e}"))?;
        // SAFETY: a NUL-terminated name; the decoder is FFmpeg's, static.
        let codec = unsafe { (api.find_decoder_by_name)(c"aac".as_ptr()) };
        if codec.is_null() {
            bail!("FFmpeg's libavcodec has no aac decoder");
        }
        // SAFETY: each allocation is checked, and each freed by the drop
        // below once it is the decoder's; a null pointer frees nothing.
        let mut decoder = unsafe {
            Self {
                api,
                context: (api.alloc_context3)(codec),
                packet: (api.packet_alloc)(),
                frame: (api.frame_alloc)(),
            }
        };
        if decoder.context.is_null() || decoder.packet.is_null() || decoder.frame.is_null() {
            bail!("FFmpeg could not allocate a decoder");
        }
        decoder.configure(codec)?;
        Ok(decoder)
    }

    /// Gives the context the AudioSpecificConfig, through parameters, so
    /// none of the context's own fields are touched, and opens it.
    fn configure(&mut self, codec: *const c_void) -> Result<()> {
        let api = self.api;
        // SAFETY: the parameters are FFmpeg's, freed with their extradata
        // below; extradata is FFmpeg's allocation, zero-padded as it asks.
        // The codec type and ID stay unknown, for open2 to take the
        // decoder's.
        unsafe {
            let mut parameters = (api.parameters_alloc)();
            if parameters.is_null() {
                bail!("FFmpeg could not allocate codec parameters");
            }
            let config = AUDIO_SPECIFIC_CONFIG;
            let extradata = (api.mallocz)(config.len() + PADDING).cast::<u8>();
            if extradata.is_null() {
                (api.parameters_free)(&mut parameters);
                bail!("FFmpeg could not allocate the AudioSpecificConfig");
            }
            std::ptr::copy_nonoverlapping(config.as_ptr(), extradata, config.len());
            (*parameters).extradata = extradata;
            (*parameters).extradata_size = config.len() as c_int;
            let copied = (api.parameters_to_context)(self.context, parameters);
            (api.parameters_free)(&mut parameters);
            if copied < 0 {
                bail!("FFmpeg did not take the AudioSpecificConfig ({copied})");
            }
            let opened = (api.open2)(self.context, codec, std::ptr::null_mut());
            if opened < 0 {
                bail!("FFmpeg's aac decoder did not open for AAC-ELD ({opened})");
            }
        }
        Ok(())
    }

    /// Decodes one AAC-ELD frame, appending its samples to `out` as floats
    /// from -1 to 1. A frame that does not decode is an error, and adds
    /// nothing.
    pub fn decode(&mut self, frame: &[u8], out: &mut Vec<f32>) -> Result<()> {
        let api = self.api;
        let len = c_int::try_from(frame.len()).context("a frame too long")?;
        // SAFETY: the packet takes the buffer, FFmpeg's and zero-padded, and
        // frees it at its unref; a buffer it did not take is freed here.
        unsafe {
            let data = (api.mallocz)(frame.len() + PADDING).cast::<u8>();
            if data.is_null() {
                bail!("FFmpeg could not allocate a packet");
            }
            std::ptr::copy_nonoverlapping(frame.as_ptr(), data, frame.len());
            if (api.packet_from_data)(self.packet, data, len) < 0 {
                (api.free)(data.cast());
                bail!("FFmpeg did not take the packet");
            }
            let sent = (api.send_packet)(self.context, self.packet);
            (api.packet_unref)(self.packet);
            if sent < 0 {
                bail!("the frame did not decode ({sent})");
            }
        }
        loop {
            // SAFETY: the frame is ours; its first fields are read only
            // while it holds a decoded frame, before its unref.
            unsafe {
                let received = (api.receive_frame)(self.context, self.frame);
                if received == AVERROR_EAGAIN {
                    return Ok(());
                }
                if received < 0 {
                    bail!("the frame did not decode ({received})");
                }
                let decoded = &*self.frame;
                let samples = usize::try_from(decoded.nb_samples).unwrap_or(0);
                let first = decoded.data[0];
                let read = match decoded.format {
                    _ if first.is_null() => Err(()),
                    SAMPLE_FMT_FLT | SAMPLE_FMT_FLTP => {
                        let floats = std::slice::from_raw_parts(first.cast::<f32>(), samples);
                        out.extend_from_slice(floats);
                        Ok(())
                    }
                    SAMPLE_FMT_S16 | SAMPLE_FMT_S16P => {
                        let shorts = std::slice::from_raw_parts(first.cast::<i16>(), samples);
                        out.extend(shorts.iter().map(|&s| f32::from(s) / 32768.0));
                        Ok(())
                    }
                    _ => Err(()),
                };
                let format = decoded.format;
                (api.frame_unref)(self.frame);
                if read.is_err() {
                    bail!("FFmpeg decoded to sample format {format}, which is not read here");
                }
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: each is FFmpeg's, freed once; a null one frees nothing.
        unsafe {
            (self.api.frame_free)(&mut self.frame);
            (self.api.packet_free)(&mut self.packet);
            (self.api.free_context)(&mut self.context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With a libavcodec on the machine, its decoder opens with the
    /// microphone's AudioSpecificConfig, and bytes that are no AAC-ELD frame
    /// crash nothing and add no more than a frame's worth of samples a call.
    /// Without one, the reason names libavcodec.
    #[test]
    fn opens_and_refuses_what_is_no_frame() {
        match Decoder::new() {
            Ok(mut decoder) => {
                let mut out = Vec::new();
                // Whatever FFmpeg makes of it, nothing past the frame is read.
                let _ = decoder.decode(&[0xFF; 70], &mut out);
                let _ = decoder.decode(&[], &mut out);
                assert!(out.len() <= 2 * crate::aap::mic::FRAME_SAMPLES);
            }
            Err(e) => {
                let reason = format!("{e:#}");
                assert!(reason.contains("libavcodec"), "{reason}");
                eprintln!("no libavcodec here: {reason}");
            }
        }
    }
}
