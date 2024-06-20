#![no_std]
//! MQTT Request/response Handling
//!
//! # Overview
//! This library is intended to be an easy way to handle inbound requests. You can subscribe to
//! topics belonging to some prefix, and Minireq will ensure that these topics are published to the
//! MQTT broker upon connection to handle discoverability.
//!
//! Minireq also simplifies the process of generating responses to the inbound request
//! automatically.
//!
//! ## Example
//! ```no_run
//! use embedded_io::Write;
//! # use embedded_nal::TcpClientStack;
//!
//! #[derive(serde::Serialize, serde::Deserialize)]
//! struct Request {
//!     data: u32,
//! }
//!
//! // Handler function for processing an incoming request.
//! pub fn test_handler(
//!     data: &[u8],
//!     mut output_buffer: &mut [u8]
//! ) -> Result<usize, &'static str> {
//!     // Deserialize the request.
//!     let mut request: Request = serde_json_core::from_slice(data).unwrap().0;
//!
//!     let response = request.data.wrapping_add(1);
//!     let start = output_buffer.len();
//!
//!     write!(output_buffer, "{}", response).unwrap();
//!     Ok(start - output_buffer.len())
//! }
//!
//! # let mut buffer = [0u8; 1024];
//! # let stack = std_embedded_nal::Stack;
//! # let localhost = embedded_nal::IpAddr::V4(embedded_nal::Ipv4Addr::new(127, 0, 0, 1));
//! let mqtt: minimq::Minimq<'_, _, _, minimq::broker::IpBroker> = minireq::minimq::Minimq::new(
//! // Constructor
//! #     stack,
//! #     std_embedded_time::StandardClock::default(),
//! #     minimq::ConfigBuilder::new(localhost.into(), &mut buffer).keepalive_interval(60),
//! );
//!
//! // Construct the client
//! let mut client: minireq::Minireq<_, _, _> = minireq::Minireq::new(
//!     "prefix/device",
//!     mqtt,
//! )
//! .unwrap();
//!
//! // We want to listen for any messages coming in on the "prefix/device/command/test" topic
//! client.subscribe("test").unwrap();
//!
//! // ...
//!
//! loop {
//!     // In your main execution loop, continually poll the client to process incoming requests.
//!     client.poll(|command, data, buffer| {
//!         match command {
//!             "test" => test_handler(data, buffer),
//!             _ => unreachable!(),
//!         }
//!     }).unwrap();
//! }
//! ```
//!
use core::fmt::Write as CoreWrite;
use core::str::FromStr;
use embedded_io::Write;

pub use minimq;

use minimq::{embedded_nal::TcpClientStack, embedded_time, QoS};

use heapless::String;
use log::{info, warn};

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
    SubscribeFailed,
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

#[derive(Copy, Clone, Debug)]
pub struct HandlerMeta {
    republished: bool,
}

pub type HandlerSlot<'a> = Option<(&'a str, HandlerMeta)>;

/// MQTT request/response interface.
pub struct Minireq<'a, Stack, Clock, Broker, const SLOTS: usize = 10>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
{
    machine: sm::StateMachine<MinireqContext<'a, Stack, Clock, Broker, SLOTS>>,
}

impl<'a, Stack, Clock, Broker, const SLOTS: usize> Minireq<'a, Stack, Clock, Broker, SLOTS>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
{
    /// Construct a new MQTT request handler.
    ///
    /// # Args
    /// * `device_prefix` - The MQTT device prefix to use for this device.
    pub fn new(
        device_prefix: &str,
        mqtt: minimq::Minimq<'a, Stack, Clock, Broker>,
    ) -> Result<Self, Error<Stack::Error>> {
        // Note(unwrap): The user must provide a prefix of the correct size.
        let mut prefix: String<MAX_TOPIC_LENGTH> = String::new();
        write!(&mut prefix, "{}/command", device_prefix).map_err(|_| Error::PrefixTooLong)?;

        Ok(Self {
            machine: sm::StateMachine::new(MinireqContext {
                handlers: [None; SLOTS],
                mqtt,
                prefix,
            }),
        })
    }
}

impl<'a, Stack, Clock, Broker> Minireq<'a, Stack, Clock, Broker>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
{
    /// Associate a handler to be called when receiving the specified request.
    ///
    /// # Args
    /// * `topic` - The command topic to subscribe to. This is appended to the string
    /// `<prefix>/command/`.
    pub fn subscribe(&mut self, topic: &'a str) -> Result<(), Error<Stack::Error>> {
        let mut added = false;
        for slot in self.machine.context_mut().handlers.iter_mut() {
            if let Some((handle, _handler)) = &slot {
                if handle == &topic {
                    return Err(Error::SubscribeFailed);
                }
            }

            if slot.is_none() {
                slot.replace((topic, HandlerMeta { republished: false }));
                added = true;
                break;
            }
        }

        if !added {
            return Err(Error::SubscribeFailed);
        }

        // Force a republish of the newly-subscribed command after adding it. We ignore failures of
        // event processing here since that would imply we are adding the handler before we've even
        // gotten to the republish state.
        self.machine
            .process_event(sm::Events::PendingRepublish)
            .ok();

        Ok(())
    }

    fn _handle_mqtt<E, F>(&mut self, mut f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(&str, &[u8], &mut [u8]) -> Result<usize, E>,
        E: core::fmt::Display,
    {
        let MinireqContext {
            handlers,
            mqtt,
            prefix,
            ..
        } = self.machine.context_mut();

        let result = mqtt.poll(|client, topic, message, properties| {
            let Some(path) = topic.strip_prefix(prefix.as_str()) else {
                info!("Unexpected MQTT topic: {}", topic);
                return;
            };

            // For paths, we do not want to include the leading slash.
            let path = path.strip_prefix('/').unwrap_or(path);

            let found_handler = handlers.iter().flatten().any(|handler| handler.0 == path);
            if !found_handler {
                if let Ok(response) = minimq::Publication::new("Command is not subscribed")
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

            let Ok(response) = minimq::DeferredPublication::new(|buf| f(path, message, buf))
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
        });

        match result {
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
    /// * `f` - A function that will be called with the incoming request topic, data, and
    /// response buffer. This function is responsible for calling the handler.
    pub fn poll<E, F>(&mut self, f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(&str, &[u8], &mut [u8]) -> Result<usize, E>,
        E: core::fmt::Display,
    {
        if !self.machine.context_mut().mqtt.client().is_connected() {
            // Note(unwrap): It's always safe to unwrap the reset event. All states must handle it.
            self.machine.process_event(sm::Events::Reset).unwrap();
        }

        self.machine.process_event(sm::Events::Update).ok();

        self._handle_mqtt(f)
    }
}

struct MinireqContext<'a, Stack, Clock, Broker, const SLOTS: usize = 10>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
{
    handlers: [HandlerSlot<'a>; SLOTS],
    mqtt: minimq::Minimq<'a, Stack, Clock, Broker>,
    prefix: String<MAX_TOPIC_LENGTH>,
}

impl<'a, Stack, Clock, Broker, const SLOTS: usize> sm::StateMachineContext
    for MinireqContext<'a, Stack, Clock, Broker, SLOTS>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
    Broker: minimq::broker::Broker,
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
        let mut prefix: String<{ MAX_TOPIC_LENGTH + 2 }> =
            String::from_str(self.prefix.as_str()).unwrap();
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

        for (command_prefix, HandlerMeta { republished, .. }) in
            handlers.iter_mut().filter_map(|handler| {
                let Some((prefix, meta)) = handler else {
                    return None;
                };

                if !meta.republished {
                    Some((prefix, meta))
                } else {
                    None
                }
            })
        {
            // Note(unwrap): The unwrap cannot fail because of restrictions on the max topic
            // length.
            let mut topic: String<{ 2 * MAX_TOPIC_LENGTH + 1 }> =
                String::from_str(prefix.as_str()).unwrap();
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
