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
use core::fmt::Write as CoreWrite;
use embedded_io::Write;

pub use minimq;

use minimq::{embedded_nal::TcpClientStack, embedded_time, QoS};

use heapless::String;
use log::{info, warn};

#[derive(Copy, Clone, Debug)]
pub struct NoErrors;

impl core::fmt::Display for NoErrors {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        unimplemented!()
    }
}

pub enum ResponseCode {
    Ok,
    Error,
}

impl ResponseCode {
    pub fn to_user_property(self) -> minimq::Property<'static> {
        let code = match self {
            ResponseCode::Ok => "Ok",
            ResponseCode::Error => "Error",
        };

        minimq::Property::UserProperty(
            minimq::types::Utf8String("code"),
            minimq::types::Utf8String(code),
        )
    }
}

// The maximum topic length of any settings path.
const MAX_TOPIC_LENGTH: usize = 128;

#[derive(Debug, PartialEq)]
pub enum Error<E> {
    RegisterFailed,
    PrefixTooLong,
    Mqtt(minimq::Error<E>),
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

type Handler<Context, E> = fn(&mut Context, &str, &[u8], &mut [u8]) -> Result<usize, E>;

#[derive(Copy, Clone, Debug)]
pub struct HandlerMeta<Context, E> {
    handler: Handler<Context, E>,
    republished: bool,
}

pub type HandlerSlot<'a, Context, E> = Option<(&'a str, HandlerMeta<Context, E>)>;

/// MQTT request/response interface.
pub struct Minireq<'a, Context, Stack, Clock, Broker, E = NoErrors>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
    E: core::fmt::Display,
{
    machine: sm::StateMachine<MinireqContext<'a, Context, Stack, Clock, Broker, E>>,
}

impl<'a, Context, Stack, Clock, Broker, E> Minireq<'a, Context, Stack, Clock, Broker, E>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
    E: core::fmt::Display,
{
    /// Construct a new MQTT request handler.
    ///
    /// # Args
    /// * `device_prefix` - The MQTT device prefix to use for this device.
    pub fn new(
        device_prefix: &str,
        mqtt: minimq::Minimq<'a, Stack, Clock, Broker>,
        handlers: &'a mut [HandlerSlot<'a, Context, E>],
    ) -> Result<Self, Error<Stack::Error>> {
        // Note(unwrap): The user must provide a prefix of the correct size.
        let mut prefix: String<MAX_TOPIC_LENGTH> = String::new();
        write!(&mut prefix, "{}/command", device_prefix).map_err(|_| Error::PrefixTooLong)?;

        Ok(Self {
            machine: sm::StateMachine::new(MinireqContext {
                handlers,
                mqtt,
                prefix,
            }),
        })
    }
}

impl<'a, Context, Stack, Clock, Broker, E> Minireq<'a, Context, Stack, Clock, Broker, E>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
    E: core::fmt::Display,
{
    /// Associate a handler to be called when receiving the specified request.
    ///
    /// # Args
    /// * `topic` - The request to register the provided handler with.
    /// * `handler` - The handler function to be called when the request occurs.
    pub fn register(
        &mut self,
        topic: &'a str,
        handler: Handler<Context, E>,
    ) -> Result<(), Error<Stack::Error>> {
        let mut added = false;
        for slot in self.machine.context_mut().handlers.iter_mut() {
            if let Some((handle, _handler)) = &slot {
                if handle == &topic {
                    return Err(Error::RegisterFailed);
                }
            }

            if slot.is_none() {
                slot.replace((
                    topic,
                    HandlerMeta {
                        handler,
                        republished: false,
                    },
                ));
                added = true;
                break;
            }
        }

        if !added {
            return Err(Error::RegisterFailed);
        }

        // Force a republish of the newly-registered command after adding it. We ignore failures of
        // event processing here since that would imply we are adding the handler before we've even
        // gotten to the republish state.
        self.machine
            .process_event(sm::Events::PendingRepublish)
            .ok();

        Ok(())
    }

    fn _handle_mqtt<F>(&mut self, mut f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(Handler<Context, E>, &str, &[u8], &mut [u8]) -> Result<usize, E>,
    {
        let MinireqContext {
            handlers,
            mqtt,
            prefix,
            ..
        } = self.machine.context_mut();

        match mqtt.poll(|client, topic, message, properties| {
            let Some(path) = topic.strip_prefix(prefix.as_str()) else {
                info!("Unexpected MQTT topic: {}", topic);
                return;
            };

            // For paths, we do not want to include the leading slash.
            let path = path.strip_prefix("/").unwrap_or(path);
            // Perform the action
            let Some(meta) = handlers.iter().find_map(|x| {
                x.as_ref()
                    .map(|(handle, handler)| if handle == &path { Some(handler) } else { None })
                    .flatten()
            }) else {
                if let Ok(response) = minimq::Publication::new("No registered handler".as_bytes())
                    .properties(&[ResponseCode::Error.to_user_property()])
                    .reply(properties)
                    .qos(QoS::AtLeastOnce)
                    .finish()
                {
                    client.publish(response).ok();
                }

                return;
            };

            // Assume that the update will succeed. We will handle conditions when it doesn't later
            // when attempting transmission.
            let props = [ResponseCode::Ok.to_user_property()];

            let Ok(response) =
                minimq::DeferredPublication::new(|buf| f(meta.handler, path, message, buf))
                    .reply(properties)
                    .properties(&props)
                    .qos(QoS::AtLeastOnce)
                    .finish()
            else {
                warn!("No response topic was provided with request: `{}`", path);
                return;
            };

            if let Err(minimq::PubError::Serialization(err)) = client.publish(response) {
                if let Ok(message) = minimq::DeferredPublication::new(|mut buf| {
                    let start = buf.len();
                    write!(buf, "{}", err).and_then(|_| Ok(start - buf.len()))
                })
                .properties(&[ResponseCode::Error.to_user_property()])
                .reply(properties)
                .qos(QoS::AtLeastOnce)
                .finish()
                {
                    // Try to send the error as a best-effort. If we don't have enough
                    // buffer space to encode the error, there's nothing more we can do.
                    client.publish(message).ok();
                };
            }
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
    pub fn poll<F>(&mut self, mut f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(Handler<Context, E>, &str, &[u8], &mut [u8]) -> Result<usize, E>,
    {
        if !self.machine.context_mut().mqtt.client().is_connected() {
            // Note(unwrap): It's always safe to unwrap the reset event. All states must handle it.
            self.machine.process_event(sm::Events::Reset).unwrap();
        }

        self.machine.process_event(sm::Events::Update).ok();

        self._handle_mqtt(f)
    }
}

struct MinireqContext<'a, Context, Stack, Clock, Broker, E>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
    E: core::fmt::Display,
{
    handlers: &'a mut [HandlerSlot<'a, Context, E>],
    mqtt: minimq::Minimq<'a, Stack, Clock, Broker>,
    prefix: String<MAX_TOPIC_LENGTH>,
}

impl<'a, Context, Stack, Clock, Broker, E> sm::StateMachineContext
    for MinireqContext<'a, Context, Stack, Clock, Broker, E>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
    E: core::fmt::Display,
{
    /// Reset the republish state of all of the handlers.
    fn reset(&mut self) {
        for HandlerMeta {
            ref mut republished,
            ..
        } in self
            .handlers
            .iter_mut()
            .filter_map(|x| x.as_mut().map(|(_topic, meta)| meta))
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

        for (
            command_prefix,
            HandlerMeta {
                ref mut republished,
                ..
            },
        ) in handlers.iter_mut().filter_map(|handler| {
            if let Some(handler) = handler {
                Some(handler)
            } else {
                None
            }
        }) {
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
