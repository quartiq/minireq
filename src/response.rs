use core::fmt::{Debug, Write};
use serde::{Deserialize, Serialize};

use heapless::String;

/// Responses are always generated as a result of handling an in-bound request.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Response<const MAX_RESPONSE_SIZE: usize> {
    pub code: i32,
    pub data: String<MAX_RESPONSE_SIZE>,
}

impl<const MAX_RESPONSE_SIZE: usize> Response<MAX_RESPONSE_SIZE> {
    /// A response without data indicating success.
    pub fn ok() -> Self {
        Self::custom(0, "Ok")
    }

    /// A response indicating failure with some error code.
    pub fn error(err: impl Debug) -> Self {
        let mut msg: String<MAX_RESPONSE_SIZE> = String::new();
        if write!(&mut msg, "{:?}", err).is_err() {
            msg = String::from("Error");
        }
        Self::custom(-1, &msg)
    }

    /// A response with json-serialized data indicating success.
    ///
    /// # Note
    /// If the provided `response` cannot fit into the message, an error will be returned instead.
    pub fn data(response: impl Serialize) -> Self {
        let data = match serde_json_core::to_string(&response) {
            Ok(data) => data,
            Err(_) => return Self::custom(-2, "Response too large"),
        };

        Self { code: 0, data }
    }

    /// A custom response type using the provided code and message.
    pub fn custom(code: i32, message: &str) -> Self {
        let mut data = String::new();

        if data.push_str(message).is_err() {
            data = String::from("Truncated");
        }

        Self { code, data }
    }
}
