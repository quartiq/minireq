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

use log::info;

pub mod response;
pub use response::Response;

// Correlation data for command republishing.
const REPUBLISH_CORRELATION_DATA: Property = Property::CorrelationData("REPUBLISH".as_bytes());

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
            *Init + Connected / reset = Subscribing,
            Subscribing + Subscribed = Idle,
            Idle + PendingRepublish = Republishing,
            Republishing + RepublishComplete = Active,
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

pub type Minireq<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize> = sm::StateMachine<MinireqContext<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>>;

/// MQTT request/response interface.
pub struct MinireqContext<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize>
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
    MinireqContext<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>
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
        mqtt.client
            .set_keepalive_interval(KEEPALIVE_INTERVAL_SECONDS)
            .unwrap();

        // Note(unwrap): The user must provide a prefix of the correct size.
        let mut prefix: String<MAX_TOPIC_LENGTH> = String::new();
        write!(&mut prefix, "{}/command", device_prefix).map_err(|_| Error::PrefixTooLong)?;

        Ok(Self {
            handlers: heapless::LinearMap::default(),
            mqtt,
            prefix,
        })
    }
}

impl<Context, Stack, Clock, const MESSAGE_SIZE: usize, const NUM_REQUESTS: usize> sm::StateMachineContext for
    MinireqContext<Context, Stack, Clock, MESSAGE_SIZE, NUM_REQUESTS>
where
    Stack: TcpClientStack,
    Clock: embedded_time::Clock,
{
    fn reset(&mut self) {
        for HandlerMeta { ref mut republished, .. } in self.handlers.values_mut() {
            *republished = false;
        }
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
        self.context_mut().handlers
            .insert(String::from(topic), HandlerMeta { handler, republished: false })
            .map(|prev| prev.is_none())
            .map_err(|_| Error::RegisterFailed)
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
        } = self.context_mut();

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

            // Extract the response topic
            let response_topic = match properties
                .iter()
                .find(|prop| matches!(prop, Property::ResponseTopic(_)))
            {
                Some(Property::ResponseTopic(topic)) => topic,
                _ => {
                    info!("No response topic was provided with request: `{}`", path);
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
                self.process_event(sm::Events::Reset).unwrap();
                Ok(())
            }
            Err(other) => Err(Error::Mqtt(other)),
        }
    }

    fn _handle_republish(&mut self) {
        let MinireqContext {
            mqtt,
            handlers,
            prefix,
            ..
        } = self.context_mut();

        for (command_prefix, HandlerMeta { republished, .. })  in handlers.iter_mut().filter(|(_, HandlerMeta { republished, .. })| !republished ) {

            // Note(unwrap): The unwrap cannot fail because of restrictions on the max topic
            // length.
            let topic: String<{2 * MAX_TOPIC_LENGTH}> = String::from(prefix.as_str());
            prefix.push_str(command_prefix).unwrap();

            if mqtt.client
                .publish(
                    &topic,
                    "TODO".as_bytes(),
                    // TODO: When Minimq supports more QoS levels, this should be increased to
                    // ensure that the client has received it at least once.
                    QoS::AtMostOnce,
                    Retain::Retained,
                    &[REPUBLISH_CORRELATION_DATA],
                )
                .is_err() {
                break;
            }

            *republished = true;
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
        if !self.context_mut().mqtt.client.is_connected() {
            // Note(unwrap): It's always safe to unwrap the reset event. All states must handle it.
            self.process_event(sm::Events::Reset).unwrap();
        }

        match *self.state() {
            sm::States::Init => {
                if self.context_mut().mqtt.client.is_connected() {
                    // Note(unwrap): It's always safe to process this event in the INIT state.
                    self.process_event(sm::Events::Connected).unwrap();
                }
            }
            sm::States::Subscribing => {
                // Note(unwrap): We ensure that this storage is always sufficiently large to store
                // the wildcard post-fix for MQTT.
                let mut prefix: String<{ MAX_TOPIC_LENGTH + 2 }> =
                    String::from(self.context().prefix.as_str());
                prefix.push_str("/#").unwrap();

                if self.context_mut().mqtt.client.subscribe(&prefix, &[]).is_ok() {
                    // Note(unwrap): It is always safe to process a Subscribed event in this state.
                    self.process_event(sm::Events::Subscribed).unwrap();
                }
            }
            sm::States::Republishing => {
                self._handle_republish();

            }
            sm::States::Active => {}
        }

        self._handle_mqtt(f)
    }
}
