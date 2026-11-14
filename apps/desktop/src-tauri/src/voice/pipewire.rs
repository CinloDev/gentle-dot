//! The Linux microphone through PipeWire (odd/tasks/linux-pipewire-microphone.md S1-S3): a
//! capture stream that asks the session's graph for 16 kHz mono f32, so PipeWire converts and
//! resamples from whatever the source records. The stream lives on a thread of its own, the
//! only one that touches PipeWire objects (they are not `Send`); `close` wakes that thread's
//! loop through a PipeWire channel and waits for it to end.
//!
//! When no PipeWire daemon answers, `open` returns `VoiceError::NoRecorder`, so the
//! `FallbackMicrophone` around it tries the command-line recorder (`pipe`) next. Linux has no
//! microphone permission to ask for.

use super::{audio, Authorization, Microphone, Sink, Stream, VoiceError};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// How long `open` waits for the stream to be set up before giving up.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct PipeWireMicrophone {
    /// The daemon to connect to; `None` is the session's default (`PIPEWIRE_REMOTE`, else
    /// `pipewire-0`). Tests name one that does not exist.
    pub(crate) remote: Option<String>,
}

impl Microphone for PipeWireMicrophone {
    fn authorization(&self) -> Authorization {
        Authorization::Authorized
    }

    fn request_authorization(&self) -> Authorization {
        Authorization::Authorized
    }

    fn open(&self, sink: Sink) -> Result<Box<dyn Stream>, VoiceError> {
        let (ready, setup) = mpsc::channel();
        let (quit, wake) = pw::channel::channel::<()>();
        let closed = Arc::new(AtomicBool::new(false));
        let capture = {
            let remote = self.remote.clone();
            let closed = closed.clone();
            move || capture(remote, sink, closed, ready, wake)
        };
        let thread = std::thread::Builder::new()
            .name("voice-pipewire".into())
            .spawn(capture)
            .map_err(|error| VoiceError::Failed(format!("The microphone could not start: {error}")))?;
        let mut stream = PipeWireStream { quit: Some(quit), thread: Some(thread), closed };
        match setup.recv_timeout(SETUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Box::new(stream)),
            Ok(Err(error)) => {
                stream.close();
                Err(error)
            }
            Err(_) => {
                stream.close();
                Err(VoiceError::Failed("The microphone could not start: PipeWire did not answer.".into()))
            }
        }
    }
}

/// Runs on the stream's thread until `close` wakes it or the capture ends on its own.
fn capture(
    remote: Option<String>,
    sink: Sink,
    closed: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), VoiceError>>,
    wake: pw::channel::Receiver<()>,
) {
    pw::init();
    let setup = || -> Result<_, VoiceError> {
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(unavailable)?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(unavailable)?;
        let core = match remote {
            Some(name) => context.connect_rc(Some(pw::properties::properties! { *pw::keys::REMOTE_NAME => name })),
            None => context.connect_rc(None),
        }
        .map_err(unavailable)?;
        let props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Communication",
            *pw::keys::APP_NAME => "Gentle Dot",
            *pw::keys::NODE_NAME => "gentle-dot-voice",
        };
        let stream = pw::stream::StreamRc::new(core.clone(), "gentle-dot-voice", props).map_err(failed)?;
        Ok((mainloop, context, core, stream))
    };
    let (mainloop, _context, core, stream) = match setup() {
        Ok(parts) => parts,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    // The capture ended without `close`: the daemon went away or the source failed.
    let interrupted = {
        let (sink, closed, mainloop) = (sink.clone(), closed.clone(), mainloop.clone());
        move || {
            if !closed.load(Ordering::SeqCst) {
                sink.failed(&VoiceError::Interrupted.to_string());
            }
            mainloop.quit();
        }
    };
    let _core_listener = core
        .add_listener_local()
        .error({
            let interrupted = interrupted.clone();
            move |id, _seq, _res, _message| {
                if id == pw::core::PW_ID_CORE {
                    interrupted();
                }
            }
        })
        .register();
    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let interrupted = interrupted.clone();
            move |_, _, _, state| {
                if ends_capture(&state) {
                    interrupted();
                }
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data() else { return };
            let samples = samples(bytes, offset, size);
            if !samples.is_empty() {
                sink.audio(&samples);
            }
        })
        .register();
    let listener = match listener {
        Ok(listener) => listener,
        Err(error) => {
            let _ = ready.send(Err(failed(error)));
            return;
        }
    };

    let format = match format() {
        Ok(format) => format,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let Some(format) = Pod::from_bytes(&format) else {
        let _ = ready.send(Err(VoiceError::Failed("The microphone format could not be built.".into())));
        return;
    };
    let connected = stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [format],
    );
    if let Err(error) = connected {
        let _ = ready.send(Err(failed(error)));
        return;
    }

    let _wake = wake.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    if ready.send(Ok(())).is_err() {
        return;
    }
    mainloop.run();
    let _ = stream.disconnect();
    drop(listener);
}

/// The stream's own end: an error, or the daemon unlinking it.
fn ends_capture(state: &pw::stream::StreamState) -> bool {
    matches!(state, pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected)
}

/// The format asked of the graph: 16 kHz mono little-endian f32 (`audio::SAMPLE_RATE`).
fn format() -> Result<Vec<u8>, VoiceError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(audio::SAMPLE_RATE);
    info.set_channels(1);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|error| VoiceError::Failed(format!("The microphone format could not be built: {error:?}")))
}

/// The f32 samples in a buffer's valid region, clamped to the mapped bytes.
fn samples(bytes: &[u8], offset: usize, size: usize) -> Vec<f32> {
    let start = offset.min(bytes.len());
    let end = start.saturating_add(size).min(bytes.len());
    let (whole, _) = bytes[start..end].as_chunks::<4>();
    whole.iter().map(|sample| f32::from_le_bytes(*sample)).collect()
}

/// No daemon to talk to: the fallback recorder may still work.
fn unavailable(_: pw::Error) -> VoiceError {
    VoiceError::NoRecorder
}

fn failed(error: pw::Error) -> VoiceError {
    VoiceError::Failed(format!("The microphone could not start: {error}"))
}

struct PipeWireStream {
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    closed: Arc<AtomicBool>,
}

impl Stream for PipeWireStream {
    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(quit) = self.quit.take() {
            // The loop may already have ended on its own; then the send has no reader.
            let _ = quit.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PipeWireStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fake::{FakeClock, FakeRecognizer, Recorder};
    use crate::voice::Voice;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    fn voice(mic: PipeWireMicrophone) -> (Voice, Arc<FakeRecognizer>) {
        let recognizer = Arc::new(FakeRecognizer::granted());
        *recognizer.final_text.lock().unwrap() = Some("done".into());
        let voice = Voice::new(Arc::new(mic), recognizer.clone(), Arc::new(FakeClock(AtomicU64::new(0))));
        (voice, recognizer)
    }

    #[test]
    fn a_buffer_is_read_as_little_endian_floats_within_its_chunk() {
        let bytes: Vec<u8> = [9.0f32, 0.5, -0.25, 1.0].iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(samples(&bytes, 4, 8), vec![0.5, -0.25]);
        assert_eq!(samples(&bytes, 4, 14), vec![0.5, -0.25, 1.0], "a partial trailing sample is dropped");
        assert_eq!(samples(&bytes, 12, 64), vec![1.0], "a chunk past the mapping is clamped");
        assert_eq!(samples(&bytes, 99, 4), Vec::<f32>::new());
    }

    #[test]
    fn the_format_asks_for_16_khz_mono_float() {
        let bytes = format().unwrap();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.parse(pod).unwrap();
        assert_eq!(info.format(), spa::param::audio::AudioFormat::F32LE);
        assert_eq!(info.rate(), audio::SAMPLE_RATE);
        assert_eq!(info.channels(), 1);
        assert_eq!(info.position()[0], spa::sys::SPA_AUDIO_CHANNEL_MONO);
    }

    #[test]
    fn only_an_error_or_an_unlinked_stream_ends_the_capture() {
        use pw::stream::StreamState;
        assert!(ends_capture(&StreamState::Error("gone".into())));
        assert!(ends_capture(&StreamState::Unconnected));
        assert!(!ends_capture(&StreamState::Connecting));
        assert!(!ends_capture(&StreamState::Paused));
        assert!(!ends_capture(&StreamState::Streaming));
    }

    #[test]
    fn no_pipewire_daemon_is_no_recorder_so_the_fallback_runs() {
        let mic = PipeWireMicrophone { remote: Some("gentle-dot-no-such-pipewire".into()) };
        let (voice, _) = voice(mic);
        let started = Instant::now();
        assert_eq!(voice.start(None, Recorder::default().events()), Err(VoiceError::NoRecorder));
        assert!(started.elapsed() < SETUP_TIMEOUT, "a missing daemon is reported at once");
    }

    /// Needs a running PipeWire with a source (docs/linux-testing.md): records for a moment
    /// from the default source and stops.
    #[test]
    #[ignore]
    fn live_pipewire_smoke() {
        let (voice, recognizer) = voice(PipeWireMicrophone::default());
        voice.start(None, Recorder::default().events()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while recognizer.log.lock().unwrap().fed.len() < audio::SAMPLE_RATE as usize / 2 {
            assert!(Instant::now() < deadline, "half a second of PCM arrives");
            std::thread::sleep(Duration::from_millis(10));
        }
        let stopped = Instant::now();
        assert_eq!(voice.stop().unwrap().text, "done");
        assert!(stopped.elapsed() < Duration::from_secs(2), "stop ends the stream's thread");
    }
}
