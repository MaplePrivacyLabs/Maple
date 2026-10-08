use pi_ai::utils::event_stream::EventStream;

mod rust_adaptations {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn completion_callbacks_may_reenter_the_stream() {
        use pi_ai::utils::event_stream::EventStreamWriter;
        use std::sync::{Arc, Mutex};
        let writer_slot = Arc::new(Mutex::new(None::<EventStreamWriter<i32>>));
        let callback_slot = writer_slot.clone();
        let mut stream = EventStream::new(
            move |_| {
                let writer = callback_slot.lock().unwrap().as_ref().unwrap().clone();
                writer.end(Some(9));
                false
            },
            |event| *event,
        );
        *writer_slot.lock().unwrap() = Some(stream.writer());
        stream.push(1);
        assert_eq!(stream.result().await, 9);
        assert_eq!(stream.next().await, Some(1));
        assert_eq!(stream.next().await, None);
        writer_slot.lock().unwrap().take();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_closed_iterator_stays_closed_when_reentry_buffers_a_later_event() {
        use pi_ai::utils::event_stream::EventStreamWriter;
        use std::sync::{Arc, Mutex};
        let writer_slot = Arc::new(Mutex::new(None::<EventStreamWriter<i32>>));
        let callback_slot = writer_slot.clone();
        let stream = EventStream::new(
            move |_| {
                callback_slot.lock().unwrap().as_ref().unwrap().end(None);
                false
            },
            |event| *event,
        );
        *writer_slot.lock().unwrap() = Some(stream.writer());
        let mut closed = stream.clone();
        let waiting = closed.next();
        stream.push(1);
        assert_eq!(waiting.await, None);
        assert_eq!(closed.next().await, None);
        let mut fresh = stream.clone();
        assert_eq!(fresh.next().await, Some(1));
        assert_eq!(fresh.next().await, None);
        writer_slot.lock().unwrap().take();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inherent_next_resumes_an_existing_stream_trait_registration() {
        let mut stream = EventStream::new(|_: &i32| false, |event| *event);
        {
            let next = futures_util::StreamExt::next(&mut stream);
            tokio::pin!(next);
            assert!(futures_util::poll!(&mut next).is_pending());
        }
        stream.push(1);
        assert_eq!(stream.next().await, Some(1));
        stream.end(None);
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_final_resolution_is_retained_and_end_without_result_stays_pending() {
        let completed = EventStream::new(|event: &i32| *event == 3, |event| *event);
        completed.push(3);
        completed.end(Some(9));
        assert_eq!(completed.result().await, 3);

        let ended = EventStream::new(|_: &i32| false, |event| *event);
        let result = ended.result();
        tokio::pin!(result);
        ended.end(None);
        assert!(futures_util::poll!(&mut result).is_pending());
        drop(ended);
        assert!(futures_util::poll!(&mut result).is_pending());
        assert!(futures_util::poll!(&mut result).is_pending());
    }
}

#[allow(clippy::module_inception)] // Preserve the upstream describe path.
mod event_stream {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn drains_buffered_events_in_order_and_ignores_events_pushed_after_completion() {
        let mut stream = EventStream::new(|event: &i32| *event == 3, |event| *event);
        for event in [1, 2, 3, 4] {
            stream.push(event);
        }
        assert_eq!(stream.result().await, 3);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        assert_eq!(events, [1, 2, 3]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn preserves_order_when_events_arrive_after_buffered_draining_starts() {
        let mut stream = EventStream::new(|_: &i32| false, |event| *event);
        stream.push(1);
        stream.push(2);
        assert_eq!(stream.next().await, Some(1));
        stream.push(3);
        assert_eq!(stream.next().await, Some(2));
        assert_eq!(stream.next().await, Some(3));
        stream.end(Some(3));
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delivers_events_to_waiting_consumers_in_registration_order() {
        let stream = EventStream::new(|_: &i32| false, |event| *event);
        let mut first_iterator = stream.clone();
        let mut second_iterator = stream.clone();
        let first_event = first_iterator.next();
        let second_event = second_iterator.next();
        stream.push(1);
        stream.push(2);
        assert_eq!(first_event.await, Some(1));
        assert_eq!(second_event.await, Some(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drains_buffered_events_after_end_and_resolves_the_explicit_result() {
        let mut stream = EventStream::new(|_: &i32| false, |event| event.to_string());
        stream.push(1);
        stream.push(2);
        stream.end(Some("complete".to_owned()));
        assert_eq!(stream.result().await, "complete");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        assert_eq!(events, [1, 2]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wakes_all_waiting_consumers_when_ended_without_a_result() {
        let stream = EventStream::new(|_: &i32| false, |event| *event);
        let mut first_iterator = stream.clone();
        let mut second_iterator = stream.clone();
        let first_event = first_iterator.next();
        let second_event = second_iterator.next();
        stream.end(None);
        assert_eq!(first_event.await, None);
        assert_eq!(second_event.await, None);
    }
}
