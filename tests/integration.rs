use minimq;
use minireq::Response;
use std_embedded_nal::Stack;
use std_embedded_time::StandardClock;

async fn client_task() {
    // Construct a Minimq client to the broker for publishing requests.
    let mut mqtt: minimq::Minimq<_, _, 256, 1> = minimq::Minimq::new(
        "127.0.0.1".parse().unwrap(),
        "tester-client",
        Stack::default(),
        StandardClock::default(),
    )
    .unwrap();

    // Wait for the broker connection
    while !mqtt.client.is_connected() {
        mqtt.poll(|_client, _topic, _message, _properties| {})
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    let response_topic = "minireq/integration/device/response";
    mqtt.client.subscribe(&response_topic, &[]).unwrap();

    // Wait the other device to connect.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Configure the error variable to trigger an internal validation failure.
    let properties = [minimq::Property::ResponseTopic(&response_topic)];

    log::info!("Publishing error setting");
    mqtt.client
        .publish(
            "minireq/integration/device/command/test",
            b"true",
            minimq::QoS::AtMostOnce,
            minimq::Retain::NotRetained,
            &properties,
        )
        .unwrap();

    // Wait until we get a response to the request.
    let mut continue_testing = true;
    loop {
        mqtt.poll(|_client, _topic, message, _properties| {
            let data: Response = serde_json_core::from_slice(message).unwrap().0;
            assert!(data.code != 0);
            continue_testing = false;
        })
        .unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        if !continue_testing {
            break;
        }
    }
}

#[tokio::test]
async fn main() {
    env_logger::init();

    // Spawn a task to send MQTT messages.
    tokio::task::spawn(async move { client_task().await });

    // Construct a settings configuration interface.
    let mut interface: minireq::Minireq<bool, _, _, 256> = minireq::Minireq::new(
        Stack::default(),
        "tester-device",
        "minireq/integration/device",
        "127.0.0.1".parse().unwrap(),
        StandardClock::default(),
    )
    .unwrap();

    interface
        .register_request("test", |exit, _req, _data| {
            *exit = true;
            Ok(Response::ok())
        })
        .unwrap();

    // Update the client until the exit
    let mut should_exit = false;
    while !should_exit {
        interface
            .poll(|handler, topic, data| handler(&mut should_exit, topic, data))
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}
