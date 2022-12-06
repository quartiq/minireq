#![no_std]
//! MQTT Request/response Handling
//!
//! # Overview
//! This library is intended to be an easy way to handle inbound requests automatically.
//!
//! Handler functions can be associated with the library to be automatically called whenever a
//! specified request is received, and the handler will automatically be invoked with the request
//! data.
//!
//! ## Limitations
//! * The `poll()` function has a somewhat odd signature (using a function to provide the `Context`
//! and call the handler) due to required compatibility with RTIC and unlocked resources.
//!
//! * Handlers may only be closures that do not capture any local resources. Instead, move local
//! captures into the `Context`, which will be provided to the handler in the function call.
//!
//! ## Example
//! ```no_run
//! # use embedded_nal::TcpClientStack;
//! type Error = minireq::Error<
//!      // Your network stack error type
//! #    <std_embedded_nal::Stack as TcpClientStack>::Error
//! >;
//!
//! struct Context {}
//!
//! #[derive(serde::Serialize, serde::Deserialize)]
//! struct Request {
//!     data: u32,
//! }
//!
//! // Handler function for processing an incoming request.
//! pub fn handler(
//!     context: &mut Context,
//!     cmd: &str,
//!     data: &[u8]
//! ) -> Result<minireq::Response<128>, Error> {
//!     // Deserialize the request.
//!     let mut request: Request = minireq::serde_json_core::from_slice(data)?.0;
//!
//!     request.data = request.data.wrapping_add(1);
//!
//!     Ok(minireq::Response::data(request))
//! }
//!
//! // Construct the client
//! let mut client: minireq::Minireq<Context, _, _, 128, 1> = minireq::Minireq::new(
//!       // Constructor arguments
//! #     std_embedded_nal::Stack::default(),
//! #     "test",
//! #     "minireq",
//! #     "127.0.0.1".parse().unwrap(),
//! #     std_embedded_time::StandardClock::default(),
//! )
//! .unwrap();
//!
//! // Whenever the `/test` command is received, call the associated handler.
//! // You may add as many handlers as you would like.
//! client.register("/test", handler).unwrap();
//!
//! // ...
//!
//! loop {
//!     // In your main execution loop, continually poll the client to process incoming requests.
//!     client.poll(|handler, command, data| {
//!         let mut context = Context {};
//!         handler(&mut context, command, data)
//!     }).unwrap();
//! }
//! ```
//!
use core::fmt::Write;

use minimq::{
    embedded_nal::{IpAddr, TcpClientStack},
    embedded_time, QoS,
};

use log::{info, warn};
use serde_json_core::heapless::String;

pub mod response;
pub use response::Response;
pub use serde_json_core;

// The maximum topic length of any settings path.
const MAX_TOPIC_LENGTH: usize = 128;

// The keepalive interval to use for MQTT in seconds.
const KEEPALIVE_INTERVAL_SECONDS: u16 = 60;

#[derive(Debug, PartialEq)]
pub enum Error<E> {
    RegisterFailed,
    PrefixTooLong,
    Deserialization(serde_json_core::de::Error),
    Mqtt(minimq::Error<E>),
}

impl<E> From<serde_json_core::de::Error> for Error<E> {
    fn from(e: serde_json_core::de::Error) -> Self {
        Error::Deserialization(e)
    }
}

impl<E> From<minimq::Error<E>> for Error<E> {
    fn from(e: minimq::Error<E>) -> Self {
        Error::Mqtt(e)
    }
}

mod sm {
    smlang::statemachine! {
        transitions: {
            *Init + Update [is_connected] / reset = Subscribing,
            Subscribing + Update [subscribe] = Republishing,
            Republishing + Update [republish] = Active,

            // We can always begin republishing again from the active processing state.
            Active + PendingRepublish = Republishing,

            // All states can reset if the MQTT broker connection is lost.
            _ + Reset = Init,
        }
    }
}

type Handler<Context, E, const RESPONSE_SIZE: usize> =
    fn(&mut Context, &str, &[u8]) -> Result<Response<RESPONSE_SIZE>, Error<E>>;

struct HandlerMeta<Context, E, const RESPONSE_SIZE: usize> {
    handler: Handler<Context, E, RESPONSE_SIZE>,
    republished: bool,
}

/// MQTT request/response interface.
pub struct Minireq<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    machine: sm::StateMachine<MinireqContext<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>>,
}

impl<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize>
    Minireq<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    /// Construct a new MQTT request handler.
    ///
    /// # Args
    /// * `stack` - The network stack to use for communication.
    /// * `client_id` - The ID of the MQTT client. May be an empty string for auto-assigning.
    /// * `device_prefix` - The MQTT device prefix to use for this device.
    /// * `broker` - The IP address of the MQTT broker to use.
    /// * `clock` - The clock for managing the MQTT connection.
    pub fn new(
        stack: Stack,
        client_id: &str,
        device_prefix: &str,
        broker: IpAddr,
        clock: Clock,
    ) -> Result<Self, Error<Stack::Error>> {
        let mut mqtt = minimq::Minimq::new(broker, client_id, stack, clock)?;

        // Note(unwrap): The client was just created, so it's valid to set a keepalive interval
        // now, since we're not yet connected to the broker.
        mqtt.client()
            .set_keepalive_interval(KEEPALIVE_INTERVAL_SECONDS)
            .unwrap();

        // Note(unwrap): The user must provide a prefix of the correct size.
        let mut prefix: String<MAX_TOPIC_LENGTH> = String::new();
        write!(&mut prefix, "{}/command", device_prefix).map_err(|_| Error::PrefixTooLong)?;

        let context = MinireqContext {
            handlers: heapless::LinearMap::default(),
            mqtt,
            prefix,
        };

        Ok(Self {
            machine: sm::StateMachine::new(context),
        })
    }
}

impl<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize>
    Minireq<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    /// Associate a handler to be called when receiving the specified request.
    ///
    /// # Args
    /// * `topic` - The request to register the provided handler with.
    /// * `handler` - The handler function to be called when the request occurs.
    pub fn register(
        &mut self,
        topic: &str,
        handler: Handler<Context, Stack::Error, MESSAGE_SIZE>,
    ) -> Result<bool, Error<Stack::Error>> {
        let added = self
            .machine
            .context_mut()
            .handlers
            .insert(
                String::from(topic),
                HandlerMeta {
                    handler,
                    republished: false,
                },
            )
            .map(|prev| prev.is_none())
            .map_err(|_| Error::RegisterFailed)?;

        // Force a republish of the newly-registered command after adding it. We ignore failures of
        // event processing here since that would imply we are adding the handler before we've even
        // gotten to the republish state.
        self.machine
            .process_event(sm::Events::PendingRepublish)
            .ok();

        Ok(added)
    }

    fn _handle_mqtt<F>(&mut self, mut f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(
            Handler<Context, Stack::Error, MESSAGE_SIZE>,
            &str,
            &[u8],
        ) -> Result<Response<MESSAGE_SIZE>, Error<Stack::Error>>,
    {
        let MinireqContext {
            handlers,
            mqtt,
            prefix,
            ..
        } = self.machine.context_mut();

        match mqtt.poll(|client, topic, message, properties| {
            let path = match topic.strip_prefix(prefix.as_str()) {
                // For paths, we do not want to include the leading slash.
                Some(path) => {
                    if !path.is_empty() {
                        &path[1..]
                    } else {
                        path
                    }
                }
                None => {
                    info!("Unexpected MQTT topic: {}", topic);
                    return;
                }
            };

            // Perform the action
            let response = match handlers.get(&String::from(path)) {
                Some(meta) => f(meta.handler, path, message).unwrap_or_else(Response::error),
                None => Response::custom(-1, "Unregistered request"),
            };

            let mut serialized_response = [0u8; MESSAGE_SIZE];
            let len = match serde_json_core::to_slice(&response, &mut serialized_response) {
                Ok(len) => len,
                Err(err) => {
                    warn!(
                        "Response could not be serialized into MQTT message: {:?}",
                        err
                    );
                    return;
                }
            };

            let response = match minimq::Publication::new(&serialized_response[..len])
                .reply(properties)
                .qos(QoS::AtLeastOnce)
                .finish()
            {
                Ok(response) => response,
                _ => {
                    warn!("No response topic was provided with request: `{}`", path);
                    return;
                }
            };

            client.publish(response).ok();
        }) {
            Ok(_) => Ok(()),
            Err(minimq::Error::SessionReset) => {
                // Note(unwrap): It's always safe to unwrap the reset event. All states must handle
                // it.
                self.machine.process_event(sm::Events::Reset).unwrap();
                Ok(())
            }
            Err(other) => Err(Error::Mqtt(other)),
        }
    }

    /// Poll the request/response interface.
    ///
    /// # Args
    /// * `f` - A function that will be called with the provided handler, command, and data. This
    /// function is responsible for calling the handler with the necessary context.
    ///
    /// # Note
    /// Any incoming requests will be automatically handled using provided handlers.
    pub fn poll<F>(&mut self, f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(
            Handler<Context, Stack::Error, MESSAGE_SIZE>,
            &str,
            &[u8],
        ) -> Result<Response<MESSAGE_SIZE>, Error<Stack::Error>>,
    {
        if !self.machine.context_mut().mqtt.client().is_connected() {
            // Note(unwrap): It's always safe to unwrap the reset event. All states must handle it.
            self.machine.process_event(sm::Events::Reset).unwrap();
        }

        self.machine.process_event(sm::Events::Update).ok();

        self._handle_mqtt(f)
    }
}

struct MinireqContext<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    handlers: heapless::LinearMap<
        String<MAX_TOPIC_LENGTH>,
        HandlerMeta<Context, Stack::Error, MESSAGE_SIZE>,
        NUM_REQUESTS,
    >,
    mqtt: minimq::Minimq<Stack, Clock, MESSAGE_SIZE, 1>,
    prefix: String<MAX_TOPIC_LENGTH>,
}

impl<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize>
    sm::StateMachineContext for MinireqContext<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    /// Reset the republish state of all of the handlers.
    fn reset(&mut self) {
        for HandlerMeta {
            ref mut republished,
            ..
        } in self.handlers.values_mut()
        {
            *republished = false;
        }
    }

    /// Guard to handle subscription to the command prefix.
    ///
    /// # Returns
    /// Error if the command prefix has not yet been subscribed to.
    fn subscribe(&mut self) -> Result<(), ()> {
        // Note(unwrap): We ensure that this storage is always sufficiently large to store
        // the wildcard post-fix for MQTT.
        let mut prefix: String<{ MAX_TOPIC_LENGTH + 2 }> = String::from(self.prefix.as_str());
        prefix.push_str("/#").unwrap();

        let topic_prefix = minimq::types::TopicFilter::new(&prefix)
            .options(minimq::types::SubscriptionOptions::default().ignore_local_messages());
        self.mqtt
            .client()
            .subscribe(&[topic_prefix], &[])
            .map_err(|_| ())
    }

    /// Guard to check for an MQTT broker connection.
    ///
    /// # Returns
    /// Ok if the MQTT broker is connected, false otherwise.
    fn is_connected(&mut self) -> Result<(), ()> {
        if self.mqtt.client().is_connected() {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Guard to handle republishing all of the command information.
    ///
    /// # Returns
    /// Ok if all command information has been republished. Error if there are still more to be
    /// published.
    fn republish(&mut self) -> Result<(), ()> {
        let MinireqContext {
            mqtt,
            handlers,
            prefix,
            ..
        } = self;

        for (command_prefix, HandlerMeta { republished, .. }) in handlers
            .iter_mut()
            .filter(|(_, HandlerMeta { republished, .. })| !republished)
        {
            // Note(unwrap): The unwrap cannot fail because of restrictions on the max topic
            // length.
            let mut topic: String<{ 2 * MAX_TOPIC_LENGTH + 1 }> = String::from(prefix.as_str());
            topic.push_str("/").unwrap();
            topic.push_str(command_prefix).unwrap();

            mqtt.client()
                .publish(
                    minimq::Publication::new(b"{}")
                        .qos(QoS::AtLeastOnce)
                        .retain()
                        .topic(&topic)
                        .finish()
                        .unwrap(),
                )
                .map_err(|_| ())?;

            *republished = true;
        }

        Ok(())
    }
}
