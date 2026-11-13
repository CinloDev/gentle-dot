//! A microphone that falls back to another (odd/tasks/linux-pipewire-microphone.md S2): Linux
//! records through PipeWire and, when PipeWire is not there to answer, through the command-line
//! recorder (`pipe`). Only `VoiceError::NoRecorder` from the first means "not there"; any other
//! failure is the first microphone's own and is reported as it is. Pure, so it is tested on
//! every OS; only Linux uses it.

use super::{Authorization, Microphone, Sink, Stream, VoiceError};
use std::sync::Arc;

pub struct FallbackMicrophone {
    first: Arc<dyn Microphone>,
    then: Arc<dyn Microphone>,
}

impl FallbackMicrophone {
    pub fn new(first: Arc<dyn Microphone>, then: Arc<dyn Microphone>) -> Self {
        FallbackMicrophone { first, then }
    }
}

impl Microphone for FallbackMicrophone {
    /// Neither Linux microphone asks for a permission; the first one answers for both.
    fn authorization(&self) -> Authorization {
        self.first.authorization()
    }

    fn request_authorization(&self) -> Authorization {
        self.first.request_authorization()
    }

    fn open(&self, sink: Sink) -> Result<Box<dyn Stream>, VoiceError> {
        match self.first.open(sink.clone()) {
            Err(VoiceError::NoRecorder) => self.then.open(sink),
            opened => opened,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fake::{FakeClock, FakeRecognizer, Recorder};
    use crate::voice::Voice;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Answers `open` with a fixed result and counts the calls.
    struct Scripted {
        answer: Mutex<Option<VoiceError>>,
        opened: AtomicUsize,
        closed: Arc<AtomicUsize>,
    }

    impl Scripted {
        fn opening() -> Arc<Self> {
            Arc::new(Scripted { answer: Mutex::new(None), opened: AtomicUsize::new(0), closed: Arc::default() })
        }

        fn failing(error: VoiceError) -> Arc<Self> {
            Arc::new(Scripted { answer: Mutex::new(Some(error)), opened: AtomicUsize::new(0), closed: Arc::default() })
        }
    }

    struct Counted(Arc<AtomicUsize>);

    impl Stream for Counted {
        fn close(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Microphone for Scripted {
        fn authorization(&self) -> Authorization {
            Authorization::Authorized
        }

        fn request_authorization(&self) -> Authorization {
            Authorization::Authorized
        }

        fn open(&self, _sink: Sink) -> Result<Box<dyn Stream>, VoiceError> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            match self.answer.lock().unwrap().clone() {
                Some(error) => Err(error),
                None => Ok(Box::new(Counted(self.closed.clone()))),
            }
        }
    }

    fn voice(first: Arc<Scripted>, then: Arc<Scripted>) -> Voice {
        let recognizer = Arc::new(FakeRecognizer::granted());
        *recognizer.final_text.lock().unwrap() = Some("done".into());
        Voice::new(Arc::new(FallbackMicrophone::new(first, then)), recognizer, Arc::new(FakeClock(AtomicU64::new(0))))
    }

    #[test]
    fn the_first_microphone_records_when_it_opens() {
        let (first, then) = (Scripted::opening(), Scripted::opening());
        let voice = voice(first.clone(), then.clone());
        voice.start(None, Recorder::default().events()).unwrap();
        assert_eq!(voice.stop().unwrap().text, "done");
        assert_eq!(first.opened.load(Ordering::SeqCst), 1);
        assert_eq!(then.opened.load(Ordering::SeqCst), 0, "the fallback is not tried");
        assert_eq!(first.closed.load(Ordering::SeqCst), 1, "stop closes the first microphone's stream");
    }

    #[test]
    fn the_fallback_records_when_the_first_is_not_there() {
        let (first, then) = (Scripted::failing(VoiceError::NoRecorder), Scripted::opening());
        let voice = voice(first.clone(), then.clone());
        voice.start(None, Recorder::default().events()).unwrap();
        assert_eq!(voice.stop().unwrap().text, "done");
        assert_eq!(first.opened.load(Ordering::SeqCst), 1);
        assert_eq!(then.opened.load(Ordering::SeqCst), 1);
        assert_eq!(then.closed.load(Ordering::SeqCst), 1, "stop closes the fallback's stream");
    }

    #[test]
    fn neither_microphone_there_is_no_recorder() {
        let (first, then) = (Scripted::failing(VoiceError::NoRecorder), Scripted::failing(VoiceError::NoRecorder));
        let voice = voice(first, then.clone());
        assert_eq!(voice.start(None, Recorder::default().events()), Err(VoiceError::NoRecorder));
        assert_eq!(then.opened.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failure_of_the_first_microphone_is_reported_without_the_fallback() {
        let failure = VoiceError::Failed("The microphone could not start: busy".into());
        let (first, then) = (Scripted::failing(failure.clone()), Scripted::opening());
        let voice = voice(first, then.clone());
        assert_eq!(voice.start(None, Recorder::default().events()), Err(failure));
        assert_eq!(then.opened.load(Ordering::SeqCst), 0, "a real failure is not hidden by the fallback");
    }
}
