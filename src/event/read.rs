use std::{collections::vec_deque::VecDeque, io, time::Duration};

#[cfg(unix)]
use crate::event::source::unix::UnixInternalEventSource;
#[cfg(windows)]
use crate::event::source::windows::WindowsEventSource;
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    filter::Filter, internal::InternalEvent, source::EventSource, timeout::PollTimeout,
};

/// Can be used to read `InternalEvent`s.
pub(crate) struct InternalEventReader {
    events: VecDeque<InternalEvent>,
    source: Option<Box<dyn EventSource>>,
    skipped_events: Vec<InternalEvent>,
}

impl Default for InternalEventReader {
    fn default() -> Self {
        #[cfg(windows)]
        let source = WindowsEventSource::new();
        #[cfg(unix)]
        let source = UnixInternalEventSource::new();

        let source = source.ok().map(|x| Box::new(x) as Box<dyn EventSource>);

        InternalEventReader {
            source,
            events: VecDeque::with_capacity(32),
            skipped_events: Vec::with_capacity(32),
        }
    }
}

impl InternalEventReader {
    /// Returns a `Waker` allowing to wake/force the `poll` method to return `Ok(false)`.
    #[cfg(feature = "event-stream")]
    pub(crate) fn waker(&self) -> Waker {
        self.source.as_ref().expect("reader source not set").waker()
    }

    pub(crate) fn poll<F>(&mut self, timeout: Option<Duration>, filter: &F) -> io::Result<bool>
    where
        F: Filter,
    {
        for event in &self.events {
            if filter.eval(event) {
                return Ok(true);
            }
        }

        let event_source = match self.source.as_mut() {
            Some(source) => source,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to initialize input reader",
                ))
            }
        };

        let poll_timeout = PollTimeout::new(timeout);

        loop {
            let maybe_event = match event_source.try_read(poll_timeout.leftover()) {
                Ok(None) => None,
                Ok(Some(event)) => {
                    if filter.eval(&event) {
                        Some(event)
                    } else {
                        self.skipped_events.push(event);
                        None
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        return Ok(false);
                    }

                    return Err(e);
                }
            };

            if poll_timeout.elapsed() || maybe_event.is_some() {
                self.events.extend(self.skipped_events.drain(..));

                if let Some(event) = maybe_event {
                    self.events.push_front(event);
                    return Ok(true);
                }

                return Ok(false);
            }
        }
    }

    /// Blocks the thread until a valid `InternalEvent` can be read.
    ///
    /// Internally, we use `try_read`, which buffers the events that do not fulfill the filter
    /// conditions to prevent stalling the thread in an infinite loop.
    pub(crate) fn read<F>(&mut self, filter: &F) -> io::Result<InternalEvent>
    where
        F: Filter,
    {
        // blocks the thread until a valid event is found
        loop {
            if let Some(event) = self.try_read(filter) {
                return Ok(event);
            }

            let _ = self.poll(None, filter)?;
        }
    }

    /// Attempts to read the first valid `InternalEvent`.
    ///
    /// This function checks all events in the queue, and stores events that do not match the
    /// filter in a buffer to be added back to the queue after all items have been evaluated. We
    /// must buffer non-fulfilling events because, if added directly back to the queue, they would
    /// result in an infinite loop, rechecking events that have already been evaluated against the
    /// filter.
    pub(crate) fn try_read<F>(&mut self, filter: &F) -> Option<InternalEvent>
    where
        F: Filter,
    {
        // check all events, storing events that do not match the filter in the `skipped_events`
        // buffer to be added back later
        let mut skipped_events = Vec::new();
        let mut result = None;
        while let Some(event) = self.events.pop_front() {
            if filter.eval(&event) {
                result = Some(event);
                break;
            }

            skipped_events.push(event);
        }

        // push all skipped events back to the event queue
        self.events.extend(skipped_events);

        result
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::{collections::VecDeque, time::Duration};

    #[cfg(unix)]
    use super::super::filter::CursorPositionFilter;
    use super::{super::Event, EventSource, Filter, InternalEvent, InternalEventReader};

    #[derive(Debug, Clone)]
    pub(crate) struct InternalEventFilter;

    impl Filter for InternalEventFilter {
        fn eval(&self, _: &InternalEvent) -> bool {
            true
        }
    }

    #[test]
    fn test_poll_fails_without_event_source() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(10)), &InternalEventFilter)
            .is_err());
    }

    #[test]
    fn test_poll_returns_true_for_matching_event_in_queue_at_front() {
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10))].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn test_poll_returns_true_for_matching_event_in_queue_at_back() {
        let mut reader = InternalEventReader {
            events: vec![
                InternalEvent::Event(Event::Resize(10, 10)),
                InternalEvent::CursorPosition(10, 20),
            ]
            .into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &CursorPositionFilter).unwrap());
    }

    #[test]
    fn test_read_returns_matching_event_in_queue_at_front() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let mut reader = InternalEventReader {
            events: vec![EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_returns_matching_event_in_queue_at_back() {
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10)), CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), SKIPPED_EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_try_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader.try_read(&CursorPositionFilter).unwrap(),
            CURSOR_EVENT
        );
        assert_eq!(
            reader.try_read(&InternalEventFilter).unwrap(),
            SKIPPED_EVENT
        );
    }

    #[test]
    fn test_poll_timeouts_if_source_has_no_events() {
        let source = FakeSource::default();

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_returns_true_if_source_has_at_least_one_event() {
        let source = FakeSource::with_events(&[InternalEvent::Event(Event::Resize(10, 10))]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_reads_returns_event_if_source_has_at_least_one_event() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_read_returns_events_if_source_has_events() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_poll_returns_false_after_all_source_events_are_consumed() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", &e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_read_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .read(&InternalEventFilter)
                .err()
                .map(|e| format!("{:?}", &e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_poll_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_read_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[derive(Default)]
    struct FakeSource {
        events: VecDeque<InternalEvent>,
        error: Option<io::Error>,
    }

    impl FakeSource {
        fn new(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: Some(io::Error::new(io::ErrorKind::Other, "")),
            }
        }

        fn with_events(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: None,
            }
        }
    }

    impl EventSource for FakeSource {
        fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            // Return error if set in case there's just one remaining event
            if self.events.len() == 1 {
                if let Some(error) = self.error.take() {
                    return Err(error);
                }
            }

            // Return all events from the queue
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }

            // Return error if there're no more events
            if let Some(error) = self.error.take() {
                return Err(error);
            }

            // Timeout
            Ok(None)
        }

        #[cfg(feature = "event-stream")]
        fn waker(&self) -> super::super::sys::Waker {
            unimplemented!();
        }
    }
}
