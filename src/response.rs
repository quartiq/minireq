use core::fmt::{Debug, Write};
use serde::{Deserialize, Serialize};

use heapless::{String, Vec};

// The maximum size of a response.
const MAX_RESPONSE_SIZE: usize = 128;

/// Responses are always generated as a result of handling an in-bound request.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub struct Response {
    pub code: i32,
    pub data: Vec<u8, MAX_RESPONSE_SIZE>,
}

impl Response {
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
        Self::custom(-1, &msg.as_str())
    }

    /// A response with json-serialized data indicating success.
    ///
    /// # Note
    /// If the provided `response` cannot fit into the message, an error will be returned instead.
    pub fn data(response: impl Serialize) -> Self {
        let mut data = Vec::new();
        data.resize(data.capacity(), 0).unwrap();
        let len = match serde_json_core::to_slice(&response, &mut data[..]) {
            Ok(len) => len,
            Err(_) => return Self::custom(-2, "Response too large"),
        };

        data.resize(len, 0).unwrap();

        Self { code: 0, data }
    }

    /// A custom response type using the provided code and message.
    pub fn custom(code: i32, message: &str) -> Self {
        let mut data = Vec::new();

        if data.write_str(message).is_err() {
            // Note(unwrap): This string should always fit in the data vector.
            data.write_str("Truncated").unwrap();
        }

        Self { code, data }
    }
}
