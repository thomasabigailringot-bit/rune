//! The AAC decoder as a Media Foundation transform: ADTS frames in, interleaved 32-bit float out.

use super::super::mf::{Library, Sound};
use std::mem::ManuallyDrop;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

pub(super) struct Audio {
    lib: &'static Library,
    mft: IMFTransform,
    pub sound: Sound,
    /// The transform's output is 16-bit integers, which `pull` converts.
    int16: bool,
}

/// What an ADTS header says (`profile` is the object type less one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Adts {
    pub rate: u32,
    pub channels: u16,
    pub profile: u8,
}

/// The first ADTS frame's header in `data`.
pub(super) fn adts(data: &[u8]) -> Option<Adts> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    (data.len() >= 7 && data[0] == 0xff && data[1] & 0xf0 == 0xf0).then(|| Adts {
        rate: RATES
            .get(usize::from((data[2] >> 2) & 0xf))
            .copied()
            .unwrap_or(48000),
        channels: u16::from(((data[2] & 1) << 2) | (data[3] >> 6)).max(1),
        profile: data[2] >> 6,
    })
}

impl Audio {
    pub(super) fn new(lib: &'static Library, header: Adts) -> Result<Self, String> {
        // SAFETY: COM calls on a transform this thread makes.
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSAACDecMFT, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("no AAC decoder: {}", e.message()))?;
            let wanted = lib.media_type()?;
            wanted
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(|e| e.message())?;
            wanted
                .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
                .map_err(|e| e.message())?;
            wanted
                .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, header.rate)
                .map_err(|e| e.message())?;
            wanted
                .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(header.channels))
                .map_err(|e| e.message())?;
            wanted
                .SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
                .map_err(|e| e.message())?;
            wanted
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 1)
                .map_err(|e| e.message())?;
            wanted
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
                .map_err(|e| e.message())?;
            // HEAACWAVEINFO's tail (ADTS payload, profile level 0x29, no struct type), then the
            // two-byte AudioSpecificConfig the ADTS header stands for.
            let index = [
                96000u32, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025,
                8000, 7350,
            ]
            .iter()
            .position(|r| *r == header.rate)
            .unwrap_or(3) as u16;
            let config =
                ((u16::from(header.profile) + 1) << 11) | (index << 7) | (header.channels << 3);
            let mut user = [0u8; 14];
            user[..2].copy_from_slice(&1u16.to_le_bytes());
            user[2..4].copy_from_slice(&0x29u16.to_le_bytes());
            user[12..].copy_from_slice(&config.to_be_bytes());
            wanted
                .SetBlob(&MF_MT_USER_DATA, &user)
                .map_err(|e| e.message())?;
            mft.SetInputType(0, &wanted, 0)
                .map_err(|e| format!("AAC input type: {}", e.message()))?;
            // Float if the transform offers it, else 16-bit.
            let mut int16 = None;
            for index in 0.. {
                let Ok(t) = mft.GetOutputAvailableType(0, index) else {
                    break;
                };
                match t.GetGUID(&MF_MT_SUBTYPE) {
                    Ok(s) if s == MFAudioFormat_Float => {
                        int16 = Some(false);
                        mft.SetOutputType(0, &t, 0).map_err(|e| e.message())?;
                        break;
                    }
                    Ok(s)
                        if s == MFAudioFormat_PCM
                            && int16.is_none()
                            && mft.SetOutputType(0, &t, 0).is_ok() =>
                    {
                        int16 = Some(true);
                    }
                    _ => {}
                }
            }
            let int16 = int16.ok_or("no AAC output type")?;
            let current = mft.GetOutputCurrentType(0).map_err(|e| e.message())?;
            let channels = current
                .GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS)
                .unwrap_or(u32::from(header.channels))
                .clamp(1, 8) as u16;
            let rate = current
                .GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND)
                .unwrap_or(header.rate);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            Ok(Self {
                lib,
                mft,
                sound: Sound { channels, rate },
                int16,
            })
        }
    }

    /// Hands over one ADTS frame stamped `time` (100 ns units).
    pub(super) fn push(&mut self, frame: &[u8], time: i64) -> Result<(), String> {
        let sample = self.lib.sample_of(frame, time)?;
        // SAFETY: input to this thread's transform.
        unsafe { self.mft.ProcessInput(0, &sample, 0) }
            .map_err(|e| format!("AAC decode: {}", e.message()))
    }

    /// The next block of sound as interleaved floats and its time in 100 ns units, if any.
    pub(super) fn pull(&mut self) -> Option<(Vec<f32>, Option<i64>)> {
        // SAFETY: a plain query on this thread's transform.
        let info = unsafe { self.mft.GetOutputStreamInfo(0) }.ok()?;
        let provided = if info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0 {
            None
        } else {
            let sample = self.lib.empty_sample().ok()?;
            let buffer = self.lib.buffer(info.cbSize.max(8192)).ok()?;
            // SAFETY: the buffer is new and ours.
            unsafe { sample.AddBuffer(&buffer) }.ok()?;
            Some(sample)
        };
        let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            pEvents: ManuallyDrop::new(None),
            dwStatus: 0,
        }];
        let mut status = 0;
        // SAFETY: one output buffer for one stream; whatever the transform leaves in it is taken out below.
        let result = unsafe { self.mft.ProcessOutput(0, &mut buffer, &mut status) };
        // SAFETY: each is taken exactly once.
        let (sample, _events) = unsafe {
            (
                ManuallyDrop::take(&mut buffer[0].pSample),
                ManuallyDrop::take(&mut buffer[0].pEvents),
            )
        };
        result.ok()?;
        let sample = sample?;
        // SAFETY: the buffer is locked for the copy and unlocked after it.
        unsafe {
            let time = sample.GetSampleTime().ok();
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let (mut base, mut len) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut base, None, Some(&mut len)).ok()?;
            let values = if self.int16 {
                std::slice::from_raw_parts(base.cast::<i16>(), len as usize / 2)
                    .iter()
                    .map(|v| f32::from(*v) / 32768.0)
                    .collect()
            } else {
                std::slice::from_raw_parts(base.cast::<f32>(), len as usize / 4).to_vec()
            };
            let _ = buffer.Unlock();
            Some((values, time))
        }
    }

    pub(super) fn flush(&mut self) {
        // SAFETY: a message to this thread's transform.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}
