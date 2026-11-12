//! How every provider ends its event stream: `Done` with the finished message,
//! or the failure recorded on the message and `Error`.

use crate::event_stream::{AssistantMessageEvent, AssistantMessageEventWriter};
use crate::types::{AssistantMessage, Model, StopReason, done_reason, error_reason};
use crate::utils_inner::stream_failure::{ProviderError, record_stream_failure};

/// End a provider's event stream with its run's `result`: `Done` carrying
/// `output`, or `Error` once `output` carries the stop reason, the error text
/// `describe` gives and the stream-failure record.
pub(crate) fn end_stream(
    model: &Model,
    writer: &AssistantMessageEventWriter,
    mut output: AssistantMessage,
    result: Result<(), ProviderError>,
    describe: fn(&ProviderError) -> String,
) {
    match result {
        Ok(()) => {
            writer.push(AssistantMessageEvent::Done {
                reason: done_reason(output.stop_reason),
                message: output,
            });
            writer.end(None);
        }
        Err(error) => {
            output.stop_reason = if error == ProviderError::Aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            output.error_message = Some(describe(&error));
            record_stream_failure(
                (&model.provider, &model.id, &model.api),
                &mut output,
                &error,
            );
            writer.push(AssistantMessageEvent::Error {
                reason: error_reason(output.stop_reason),
                error: output.clone(),
            });
            writer.end(Some(output));
        }
    }
}
