//! The H.264 decoder as a Media Foundation transform: access units in, NV12 pictures out, with
//! no source reader, container or probe in between. On a D3D11 device manager the transform
//! decodes on the GPU and hands back its own textures (`Output::Texture`); without one it decodes
//! on the CPU into system memory (`Output::Memory`).
//!
//! A new SPS and PPS at a keyframe that changes the picture size comes back from `ProcessOutput`
//! as a stream change; `Video::pull` negotiates the new output type and says so (`Output::Changed`).

use super::super::mf::{self, Library, Picture};
use std::mem::ManuallyDrop;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::{VARIANT, VT_UI4};
use windows::core::Interface;

pub(super) enum Output {
    /// A decoded picture and its time in 100 ns units.
    Picture(IMFSample, i64),
    /// The transform wants more input.
    More,
    /// The output type changed (a new picture size): `Video::picture` has the new one.
    Changed,
}

pub(super) struct Video {
    lib: &'static Library,
    mft: IMFTransform,
    /// The transform allocates its output samples (it does, on a D3D11 manager).
    provides: bool,
    pub hardware: bool,
    pub picture: Option<Picture>,
    /// Access units taken in, and pictures given out.
    pub inputs: u64,
    pub outputs: u64,
    /// Inputs taken in before the first output.
    pub hold: Option<u64>,
}

impl Video {
    /// A decoder on `manager`'s device, or on the CPU when there is none or it refuses it.
    pub(super) fn new(
        lib: &'static Library,
        manager: Option<&IMFDXGIDeviceManager>,
    ) -> Result<Self, String> {
        // SAFETY: COM calls on a transform this thread makes; the input type is set before streaming.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("no H.264 decoder: {}", e.message()))?;
            if let Ok(attributes) = mft.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(api) = mft.cast::<ICodecAPI>() {
                let mut on = VARIANT::default();
                (*on.Anonymous.Anonymous).vt = VT_UI4;
                (*on.Anonymous.Anonymous).Anonymous.ulVal = 1;
                let _ = api.SetValue(&CODECAPI_AVLowLatencyMode, &on);
            }
            let hardware = manager.is_some_and(|m| {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, m.as_raw() as usize)
                    .is_ok()
            });
            let wanted = lib.media_type()?;
            wanted
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| e.message())?;
            wanted
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(|e| e.message())?;
            wanted
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| e.message())?;
            mft.SetInputType(0, &wanted, 0)
                .map_err(|e| format!("H.264 input type: {}", e.message()))?;
            let mut video = Self {
                lib,
                mft,
                provides: false,
                hardware,
                picture: None,
                inputs: 0,
                outputs: 0,
                hold: None,
            };
            // Some transforms already know their output (from the input type alone).
            let _ = video.negotiate();
            let _ = video
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = video
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(video)
        }
    }

    /// Picks NV12 as the output type, and reads the picture it describes.
    fn negotiate(&mut self) -> Result<(), String> {
        // SAFETY: type enumeration and selection on this thread's transform.
        unsafe {
            for index in 0.. {
                let Ok(t) = self.mft.GetOutputAvailableType(0, index) else {
                    return Err("no NV12 output type".into());
                };
                if t.GetGUID(&MF_MT_SUBTYPE)
                    .is_ok_and(|s| s == MFVideoFormat_NV12)
                    && self.mft.SetOutputType(0, &t, 0).is_ok()
                {
                    break;
                }
            }
            let current = self.mft.GetOutputCurrentType(0).map_err(|e| e.message())?;
            self.picture = Some(mf::picture(&current)?);
            let info = self.mft.GetOutputStreamInfo(0).map_err(|e| e.message())?;
            self.provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            Ok(())
        }
    }

    /// Hands over one access unit stamped `time` (100 ns units). `Err(false)` when the transform
    /// won't take it until its pictures are pulled.
    pub(super) fn push(&mut self, unit: &[u8], time: i64) -> Result<(), bool> {
        let sample = self.lib.sample_of(unit, time).map_err(|_| true)?;
        // SAFETY: input to this thread's transform.
        match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
            Ok(()) => {
                self.inputs += 1;
                Ok(())
            }
            Err(e) if e.code() == MF_E_NOTACCEPTING => Err(false),
            Err(_) => Err(true),
        }
    }

    /// The next picture, if the transform has one.
    pub(super) fn pull(&mut self) -> Result<Output, String> {
        let provided = if self.provides {
            None
        } else {
            Some(self.output_sample()?)
        };
        let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            pEvents: ManuallyDrop::new(None),
            dwStatus: 0,
        }];
        let mut status = 0;
        // SAFETY: one output buffer for one stream; whatever the transform puts in it is taken out below.
        let result = unsafe { self.mft.ProcessOutput(0, &mut buffer, &mut status) };
        // SAFETY: the transform filled (or left) these; each is taken exactly once.
        let (sample, _events) = unsafe {
            (
                ManuallyDrop::take(&mut buffer[0].pSample),
                ManuallyDrop::take(&mut buffer[0].pEvents),
            )
        };
        match result {
            Ok(()) => {
                let sample = sample.ok_or("the decoder gave no picture")?;
                // SAFETY: a plain read on a sample we own.
                let time = unsafe { sample.GetSampleTime() }.unwrap_or(0);
                self.outputs += 1;
                // Inputs taken in when the first picture came out, less the one it belongs to.
                self.hold.get_or_insert(self.inputs.saturating_sub(1));
                Ok(Output::Picture(sample, time))
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::More),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                self.negotiate()?;
                Ok(Output::Changed)
            }
            Err(e) => Err(format!(
                "H.264 decode: {} ({:#010x})",
                e.message(),
                e.code().0
            )),
        }
    }

    /// A system-memory sample for a transform that doesn't allocate its own.
    fn output_sample(&self) -> Result<IMFSample, String> {
        // SAFETY: a plain query on this thread's transform.
        let info = unsafe { self.mft.GetOutputStreamInfo(0) }.map_err(|e| e.message())?;
        let sample = self.lib.empty_sample()?;
        let buffer = self.lib.buffer(info.cbSize.max(1))?;
        // SAFETY: the buffer is new and ours.
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| e.message())?;
        Ok(sample)
    }

    /// Forgets what is in flight: the next unit must be a keyframe.
    pub(super) fn flush(&mut self) {
        // SAFETY: a message to this thread's transform.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
    }
}
