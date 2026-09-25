//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Collects one batch from a client library's message stream, for endpoints
//! whose client hands out messages one at a time.

use std::time::Duration;

use futures::{Stream, TryStreamExt};

/// How long a draining route waits for a batch's first message, so an idle
/// source yields the empty batch that ends it.
pub const DRAIN_FIRST_WAIT: Duration = Duration::from_millis(250);
/// How long to wait for each further message before the batch is sent as is.
pub const NEXT_MESSAGE_WAIT: Duration = Duration::from_millis(5);

/// Collects up to `max` items. A live route waits for the first item as long as
/// it takes; a draining one (`exit_on_empty`) gives up after [`DRAIN_FIRST_WAIT`]
/// and returns an empty batch. `Ok(None)` means the stream ended before any item.
pub async fn next_batch<S, T, E>(
    stream: &mut S,
    max: usize,
    exit_on_empty: bool,
) -> Result<Option<Vec<T>>, E>
where
    S: Stream<Item = Result<T, E>> + Unpin,
{
    let mut items = Vec::with_capacity(max);
    while items.len() < max {
        let next = match wait_for(items.len(), exit_on_empty) {
            Some(wait) => match tokio::time::timeout(wait, stream.try_next()).await {
                Ok(next) => next?,
                Err(_) => break,
            },
            None => stream.try_next().await?,
        };
        match next {
            Some(item) => items.push(item),
            None if items.is_empty() => return Ok(None),
            None => break,
        }
    }
    Ok(Some(items))
}

/// How long to wait for the item at `index`, or `None` to wait indefinitely.
fn wait_for(index: usize, exit_on_empty: bool) -> Option<Duration> {
    match index {
        0 if !exit_on_empty => None,
        0 => Some(DRAIN_FIRST_WAIT),
        _ => Some(NEXT_MESSAGE_WAIT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    type Items = Vec<Result<u32, String>>;

    #[test]
    fn live_consumption_waits_for_the_first_message() {
        assert_eq!(wait_for(0, false), None);
        assert_eq!(wait_for(1, false), Some(NEXT_MESSAGE_WAIT));
    }

    #[test]
    fn draining_gives_up_on_an_idle_source() {
        assert_eq!(wait_for(0, true), Some(DRAIN_FIRST_WAIT));
        assert_eq!(wait_for(1, true), Some(NEXT_MESSAGE_WAIT));
    }

    #[tokio::test]
    async fn a_batch_stops_at_max() {
        let mut source = stream::iter::<Items>(vec![Ok(1), Ok(2), Ok(3)]);
        assert_eq!(
            next_batch(&mut source, 2, false).await,
            Ok(Some(vec![1, 2]))
        );
        assert_eq!(next_batch(&mut source, 2, false).await, Ok(Some(vec![3])));
        assert_eq!(next_batch(&mut source, 2, false).await, Ok(None));
    }

    #[tokio::test]
    async fn an_error_is_returned_as_is() {
        let mut source = stream::iter::<Items>(vec![Ok(1), Err("down".into())]);
        assert_eq!(next_batch(&mut source, 5, false).await, Err("down".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_draining_route_gets_an_empty_batch_from_an_idle_source() {
        let mut source = stream::pending::<Result<u32, String>>();
        assert_eq!(next_batch(&mut source, 5, true).await, Ok(Some(vec![])));
    }
}
