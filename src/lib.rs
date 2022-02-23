#![no_std]
/// MQTT Request/response Handling
///
/// # Overview
/// This library is intended to be an easy way to handle inbound requests automatically.
///
/// Handler functions can be associated with the library to be automatically called whenever a
/// specified request is received, and the handler will automatically be invoked when the request
/// is received.
///
/// ## Limitations
/// * The `poll()` function has a somewhat odd signature (using a function to provide the `Context`
/// and call the handler) due to required compatibility with RTIC and unlocked resources.
///
/// * Handlers may only be closures that do not capture any local resources. Instead, move local
/// captures into the `Context`, which will be provided to the handler in the function call.
///
use core::fmt::Write;

use minimq::{
    embedded_nal::{IpAddr, TcpClientStack},
    embedded_time, Property, QoS, Retain,
};

use serde_json_core::heapless::{String, Vec};

use log::{info, warn};

pub mod response;
pub use response::Response;

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
            *Init + Connected = Connected,
            Connected + Subscribed = Active,
            _ + Reset = Init,
        }
    }

    pub struct Context;

    impl StateMachineContext for Context {}
}

type Handler<Context, E> = fn(&mut Context, &str, &[u8]) -> Result<Response, Error<E>>;

/// MQTT request/response interface.
pub struct Minireq<Context, Stack, Clock, const MESSAGE_SIZE: usize>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    // TODO: Maybe use a hash-based IndexMap?
    requests: heapless::LinearMap<String<60>, Handler<Context, Stack::Error>, 10>,
    mqtt: minimq::Minimq<Stack, Clock, MESSAGE_SIZE, 1>,
    prefix: String<MAX_TOPIC_LENGTH>,
    state: sm::StateMachine<sm::Context>,
}

impl<Context, Stack, Clock, const MESSAGE_SIZE: usize> Minireq<Context, Stack, Clock, MESSAGE_SIZE>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock + Clone,
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
        let mut mqtt = minimq::Minimq::new(broker, client_id, stack, clock.clone())?;

        // Note(unwrap): The client was just created, so it's valid to set a keepalive interval
        // now, since we're not yet connected to the broker.
        mqtt.client
            .set_keepalive_interval(KEEPALIVE_INTERVAL_SECONDS)
            .unwrap();

        // Note(unwrap): The user must provide a prefix of the correct size.
        let mut prefix: String<MAX_TOPIC_LENGTH> = String::new();
        write!(&mut prefix, "{}/command", device_prefix).map_err(|_| Error::PrefixTooLong)?;

        Ok(Self {
            requests: heapless::LinearMap::default(),
            mqtt,
            prefix,
            state: sm::StateMachine::new(sm::Context),
        })
    }

    /// Associate a handler to be called when receiving the specified request.
    ///
    /// # Args
    /// * `topic` - The request to register the provided handler with.
    /// * `handler` - The handler function to be called when the request occurs.
    pub fn register_request(
        &mut self,
        topic: &str,
        handler: Handler<Context, Stack::Error>,
    ) -> Result<bool, Error<Stack::Error>> {
        self.requests
            .insert(String::from(topic), handler)
            .map(|prev| prev.is_none())
            .map_err(|_| Error::RegisterFailed)
    }

    fn _handle_mqtt<F>(&mut self, mut f: F) -> Result<(), Error<Stack::Error>>
    where
        F: FnMut(
            Handler<Context, Stack::Error>,
            &str,
            &[u8],
        ) -> Result<Response, Error<Stack::Error>>,
    {
        let Self {
            requests,
            mqtt,
            prefix,
            ..
        } = self;

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

            // Extract the response topic
            let response_topic = match properties.iter().find_map(|prop| {
                if let Property::ResponseTopic(topic) = prop {
                    Some(topic)
                } else {
                    None
                }
            }) {
                Some(topic) => topic,
                None => {
                    warn!("Ignoring inbound request with no response topic");
                    return;
                }
            };

            // Extract correlation data
            let mut response_props: Vec<minimq::Property, 1> = Vec::new();
            if let Some(cd) = properties
                .iter()
                .find(|prop| matches!(prop, minimq::Property::CorrelationData(_)))
            {
                // Note(unwrap): We guarantee there is space for this item.
                response_props.push(*cd).unwrap();
            }

            // Perform the action
            let response = match requests.get(&String::from(path)) {
                Some(&handler) => f(handler, path, message).unwrap_or_else(Response::error),
                None => Response::custom(-1, "Unregistered request"),
            };

            // Note(unwrap): We currently have no means of indicating a response that is too long.
            // TODO: We should return this as an error in the future.
            let mut serialized_response = [0u8; MESSAGE_SIZE];
            let len = serde_json_core::to_slice(&response, &mut serialized_response).unwrap();

            client
                .publish(
                    response_topic,
                    &serialized_response[..len],
                    // TODO: When Minimq supports more QoS levels, this should be increased to
                    // ensure that the client has received it at least once.
                    QoS::AtMostOnce,
                    Retain::NotRetained,
                    &response_props,
                )
                .ok();
        }) {
            Ok(_) => Ok(()),
            Err(minimq::Error::SessionReset) => {
                // Note(unwrap): It's always safe to unwrap the reset event. All states must handle
                // it.
                self.state.process_event(sm::Events::Reset).unwrap();
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
            Handler<Context, Stack::Error>,
            &str,
            &[u8],
        ) -> Result<Response, Error<Stack::Error>>,
    {
        if !self.mqtt.client.is_connected() {
            // Note(unwrap): It's always safe to unwrap the reset event. All states must handle it.
            self.state.process_event(sm::Events::Reset).unwrap();
        }

        match *self.state.state() {
            sm::States::Init => {
                if self.mqtt.client.is_connected() {
                    // Note(unwrap): It's always safe to process this event in the INIT state.
                    self.state.process_event(sm::Events::Connected).unwrap();
                }
            }
            sm::States::Connected => {
                // Note(unwrap): We ensure that this storage is always sufficiently large to store
                // the wildcard post-fix for MQTT.
                let mut prefix: String<{ MAX_TOPIC_LENGTH + 2 }> =
                    String::from(self.prefix.as_str());
                prefix.push_str("/#").unwrap();

                if self.mqtt.client.subscribe(&prefix, &[]).is_ok() {
                    // Note(unwrap): It is always safe to process a Subscribed event in this state.
                    self.state.process_event(sm::Events::Subscribed).unwrap();
                }
            }
            sm::States::Active => {}
        }

        self._handle_mqtt(f)
    }
}
