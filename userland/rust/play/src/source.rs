//! Track sources: something that yields 16-bit PCM for the audio
//! device, whichever format the file is. The format is sniffed from the
//! file's first bytes (`RIFF` vs. an ID3v2 tag / MPEG frame sync), never
//! its extension.
//!
//! WAV must already be PCM, 16-bit, mono/stereo, 44100/48000 Hz --
//! exactly what the kernel's audio drivers accept. MP3 is decoded by minimp3, a
//! C library compiled in c/mp3.c (vendored unmodified in
//! vendor/minimp3/) and called through the FFI below; its
//! output must also come out at 44100/48000 Hz.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ffi::{CStr, c_int};

use anshi::{File, SEEK_CUR, SEEK_END, SEEK_SET};

/// What the player screen needs to know about a track.
pub struct TrackInfo {
    pub rate: u32,
    pub channels: u32,
    pub total_secs: u32,
    /// 0 for WAV; an MP3's first audio frame's bitrate otherwise.
    pub mp3_kbps: u32,
}

pub enum Source {
    Wav { file: File, remaining: u32 },
    Mp3(Box<Mp3Stream>),
}

impl Source {
    /// Opens `path` and sniffs its format. `name` is only for error
    /// messages.
    pub fn open(path: &CStr, name: &str) -> Result<(Source, TrackInfo), String> {
        let mut file = File::open(path).ok_or_else(|| format!("cannot open {name}"))?;
        let mut magic = [0u8; 4];
        if file.read_full(&mut magic) == 4 {
            file.seek(0, SEEK_SET);
            if &magic == b"RIFF" {
                return open_wav(file).map_err(|e| format!("{name}: {e}"));
            }
            if &magic[..3] == b"ID3" || (magic[0] == 0xFF && magic[1] & 0xE0 == 0xE0) {
                return Mp3Stream::open(file).map_err(|e| format!("{name}: {e}"));
            }
        }
        Err(format!("{name}: not a WAV or MP3 file"))
    }

    /// Fills `buf` with up to `buf.len()` bytes of PCM (`buf.len()` must
    /// be a multiple of 4, so it's always a multiple of the frame size).
    /// Returns the byte count, 0 at end of track.
    pub fn read_pcm(&mut self, buf: &mut [u8], frame_bytes: u32) -> usize {
        match self {
            Source::Wav { file, remaining } => {
                let mut want = (*remaining as usize).min(buf.len());
                want -= want % frame_bytes as usize;
                if want == 0 {
                    return 0;
                }
                let n = file.read(&mut buf[..want]);
                *remaining -= n as u32;
                n
            }
            Source::Mp3(mp3) => mp3.read_pcm(buf),
        }
    }
}

// ---------- WAV ----------

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Walks RIFF chunks looking for `fmt `/`data` rather than assuming a
/// fixed layout -- some WAV files carry a LIST/fact chunk in between.
/// Leaves `file` positioned at the start of the PCM data.
fn open_wav(mut file: File) -> Result<(Source, TrackInfo), String> {
    let bad = || String::from("not a valid WAV file");
    let mut hdr = [0u8; 12];
    if file.read_full(&mut hdr) != 12 || &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return Err(bad());
    }

    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    loop {
        let mut chunk = [0u8; 8];
        if file.read_full(&mut chunk) != 8 {
            return Err(bad()); // EOF before `data`
        }
        let size = u32le(&chunk[4..8]);
        match &chunk[0..4] {
            b"fmt " => {
                let mut f = [0u8; 16];
                if size < 16 || file.read_full(&mut f) != 16 {
                    return Err(bad());
                }
                fmt = Some((u16le(&f[0..]), u16le(&f[2..]), u32le(&f[4..]), u16le(&f[14..])));
                file.seek((size - 16) as i64 + (size & 1) as i64, SEEK_CUR);
            }
            b"data" => {
                let (format, channels, rate, bits) = fmt.ok_or_else(bad)?;
                if format != 1 || bits != 16 || !(channels == 1 || channels == 2) || !(rate == 44100 || rate == 48000) {
                    return Err(format!(
                        "unsupported format (fmt={format} bits={bits} ch={channels} rate={rate}) -- need \
                         PCM/16-bit/mono-or-stereo/44100-or-48000"
                    ));
                }
                // Some writers never patch the real length back in (0xFFFFFFFF
                // "unknown" is the classic case): clamp to what the file really
                // has left, or the duration shown would be nonsense.
                let data_start = file.seek(0, SEEK_CUR);
                let file_end = file.seek(0, SEEK_END);
                file.seek(data_start, SEEK_SET);
                let real = (file_end - data_start).max(0) as u32;
                let data_bytes = size.min(real);
                let info = TrackInfo {
                    rate,
                    channels: channels as u32,
                    total_secs: data_bytes / (rate * channels as u32 * 2),
                    mp3_kbps: 0,
                };
                return Ok((Source::Wav { file, remaining: data_bytes }, info));
            }
            _ => {
                file.seek(size as i64 + (size & 1) as i64, SEEK_CUR); // chunks are word-aligned
            }
        }
    }
}

// ---------- MP3 (minimp3 FFI) ----------

const MAX_SAMPLES_PER_FRAME: usize = 1152 * 2;

/// minimp3's mp3dec_t, field for field (vendor/minimp3/minimp3.h).
#[repr(C)]
struct Mp3Dec {
    mdct_overlap: [[f32; 9 * 32]; 2],
    qmf_state: [f32; 15 * 2 * 32],
    reserv: c_int,
    free_format_bytes: c_int,
    header: [u8; 4],
    reserv_buf: [u8; 511],
}
const _: () = assert!(size_of::<Mp3Dec>() == 6668); // sizeof(mp3dec_t) in C

#[repr(C)]
#[derive(Default)]
struct FrameInfo {
    frame_bytes: c_int,
    frame_offset: c_int,
    channels: c_int,
    hz: c_int,
    layer: c_int,
    bitrate_kbps: c_int,
}

unsafe extern "C" {
    fn mp3dec_init(dec: *mut Mp3Dec);
    fn mp3dec_decode_frame(dec: *mut Mp3Dec, mp3: *const u8, mp3_bytes: c_int, pcm: *mut i16, info: *mut FrameInfo) -> c_int;
}

/// MP3 input is streamed through a fixed buffer -- a whole song won't
/// fit under the 4 MiB brk heap cap (kernel/src/exec/syscall.rs), and
/// doesn't need to. Refilled whenever it drops below half full, so
/// minimp3 always sees the several consecutive frames it wants before
/// trusting a sync word.
const IN_BYTES: usize = 16 * 1024;

pub struct Mp3Stream {
    file: File,
    dec: Mp3Dec,
    input: Vec<u8>,
    in_pos: usize,
    in_len: usize,
    eof: bool,
    /// The span of `input` holding the frame decode_frame() last decoded.
    last_frame: (usize, usize),
    pcm: [i16; MAX_SAMPLES_PER_FRAME],
    /// Bytes of `pcm` holding decoded audio / already handed out.
    pcm_len: usize,
    pcm_pos: usize,
    rate: u32,
    channels: u32,
}

impl Mp3Stream {
    /// Skips any leading ID3v2 tag, decodes the first frame (left
    /// pending for the first read_pcm() -- the output rate/channels
    /// aren't known until then), and works out the duration.
    fn open(mut file: File) -> Result<(Source, TrackInfo), String> {
        let audio_start = id3v2_size(&mut file);
        let file_end = file.seek(0, SEEK_END);
        file.seek(audio_start, SEEK_SET);

        let mut s = Box::new(Mp3Stream {
            file,
            dec: Mp3Dec {
                mdct_overlap: [[0.0; 288]; 2],
                qmf_state: [0.0; 960],
                reserv: 0,
                free_format_bytes: 0,
                header: [0; 4],
                reserv_buf: [0; 511],
            },
            input: vec![0; IN_BYTES],
            in_pos: 0,
            in_len: 0,
            eof: false,
            last_frame: (0, 0),
            pcm: [0; MAX_SAMPLES_PER_FRAME],
            pcm_len: 0,
            pcm_pos: 0,
            rate: 0,
            channels: 0,
        });
        unsafe { mp3dec_init(&mut s.dec) };

        let mut info = FrameInfo::default();
        let samples = s.decode_frame(&mut info);
        if samples == 0 {
            return Err("no decodable MP3 audio found".into());
        }
        if info.hz != 44100 && info.hz != 48000 {
            return Err(format!("unsupported MP3 sample rate {} Hz -- need 44100 or 48000", info.hz));
        }
        s.rate = info.hz as u32;
        s.channels = info.channels as u32;
        s.pcm_len = samples * s.channels as usize * 2;

        let (start, end) = s.last_frame;
        let total_secs = match xing_frame_count(&s.input[start..end]) {
            // A VBR file's Xing/Info header carries the real frame count.
            Some(frames) => (frames as u64 * samples as u64 / info.hz as u64) as u32,
            // Otherwise assume CBR: duration is just size / bitrate.
            None if info.bitrate_kbps > 0 && file_end > audio_start => {
                ((file_end - audio_start) / (info.bitrate_kbps as i64 * 125)) as u32
            }
            None => 0,
        };

        let track = TrackInfo { rate: s.rate, channels: s.channels, total_secs, mp3_kbps: info.bitrate_kbps as u32 };
        Ok((Source::Mp3(s), track))
    }

    /// Decodes the next audio frame into `pcm`, returning its sample
    /// count per channel -- 0 at end of stream. Anything minimp3 can't
    /// decode (junk between frames, a trailing ID3v1 tag) is skipped.
    fn decode_frame(&mut self, info: &mut FrameInfo) -> usize {
        loop {
            if !self.eof && self.in_len - self.in_pos < IN_BYTES / 2 {
                self.input.copy_within(self.in_pos..self.in_len, 0);
                self.in_len -= self.in_pos;
                self.in_pos = 0;
                let n = self.file.read_full(&mut self.input[self.in_len..]);
                self.in_len += n;
                if self.in_len < IN_BYTES {
                    self.eof = true;
                }
            }

            let avail = self.in_len - self.in_pos;
            if avail == 0 {
                return 0;
            }
            let samples = unsafe {
                mp3dec_decode_frame(
                    &mut self.dec,
                    self.input.as_ptr().add(self.in_pos),
                    avail as c_int,
                    self.pcm.as_mut_ptr(),
                    info,
                )
            };
            if info.frame_bytes == 0 {
                // No complete frame anywhere in what's buffered (at least
                // half the buffer, unless at EOF) -- it's junk. Drop it.
                if self.eof {
                    return 0;
                }
                self.in_pos = self.in_len;
                continue;
            }
            let start = self.in_pos + info.frame_offset as usize;
            self.in_pos += info.frame_bytes as usize;
            self.last_frame = (start, self.in_pos);
            if samples > 0 {
                return samples as usize;
            }
        }
    }

    fn read_pcm(&mut self, buf: &mut [u8]) -> usize {
        while self.pcm_pos == self.pcm_len {
            let mut info = FrameInfo::default();
            let samples = self.decode_frame(&mut info);
            if samples == 0 {
                return 0;
            }
            // A mid-stream rate/channel change (rare, but legal MP3) can't
            // be followed without reopening the audio stream -- skip any
            // such frame rather than play it at the wrong speed.
            if info.hz as u32 != self.rate || info.channels as u32 != self.channels {
                continue;
            }
            self.pcm_pos = 0;
            self.pcm_len = samples * self.channels as usize * 2;
        }
        let n = (self.pcm_len - self.pcm_pos).min(buf.len());
        for (i, out) in buf[..n].chunks_exact_mut(2).enumerate() {
            let sample = self.pcm[self.pcm_pos / 2 + i];
            out.copy_from_slice(&sample.to_le_bytes());
        }
        self.pcm_pos += n;
        n
    }
}

/// Size of a leading ID3v2 tag (header, body, optional footer), 0 if
/// none -- skipped outright rather than left for minimp3 to scan past,
/// since embedded cover art can be hundreds of KiB.
fn id3v2_size(file: &mut File) -> i64 {
    let mut h = [0u8; 10];
    file.seek(0, SEEK_SET);
    if file.read_full(&mut h) != 10 || &h[0..3] != b"ID3" {
        return 0;
    }
    // "synchsafe": 7 bits per byte
    let body = h[6..10].iter().fold(0i64, |acc, &b| (acc << 7) | (b & 0x7F) as i64);
    10 + body + if h[5] & 0x10 != 0 { 10 } else { 0 }
}

/// A VBR file's first frame is usually a silent Xing/Info header frame
/// carrying the real frame count. It sits right after the side info,
/// whose size varies, so just look for the tag near the frame's start.
fn xing_frame_count(frame: &[u8]) -> Option<u32> {
    let limit = frame.len().saturating_sub(12).min(48);
    (4..limit).find_map(|off| {
        let tag = &frame[off..off + 4];
        if tag != b"Xing" && tag != b"Info" {
            return None;
        }
        let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let flags = be32(&frame[off + 4..]);
        if flags & 1 != 0 { Some(be32(&frame[off + 8..])) } else { None }
    })
}
