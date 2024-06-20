use std_embedded_time::StandardClock;

async fn client_task() {
    // Construct a Minimq client to the broker for publishing requests.
    let mut buffer = [0u8; 1024];
    let stack = std_embedded_nal::Stack;
    let localhost = embedded_nal::IpAddr::V4(embedded_nal::Ipv4Addr::new(127, 0, 0, 1));
    let mut mqtt: minimq::Minimq<'_, _, _, minimq::broker::IpBroker> = minimq::Minimq::new(
        stack,
        StandardClock::default(),
        minimq::ConfigBuilder::new(localhost.into(), &mut buffer).keepalive_interval(60),
    );

    // Wait for the broker connection
    while !mqtt.client().is_connected() {
        mqtt.poll(|_client, _topic, _message, _properties| {})
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    let response_topic = "minireq/integration/device/response";
    mqtt.client()
        .subscribe(&[minimq::types::TopicFilter::new(response_topic)], &[])
        .unwrap();

    // Wait the other device to connect.
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // Configure the error variable to trigger an internal validation failure.
    let properties = [minimq::Property::ResponseTopic(minimq::types::Utf8String(
        response_topic,
    ))];

    log::info!("Publishing error setting");
    mqtt.client()
        .publish(
            minimq::Publication::new(b"true")
                .topic("minireq/integration/device/command/test")
                .properties(&properties)
                .finish()
                .unwrap(),
        )
        .unwrap();

    // Wait until we get a response to the request.
    let mut continue_testing = true;
    loop {
        mqtt.poll(|_client, _topic, _message, _properties| {
            let (header, code) = properties
                .into_iter()
                .find_map(|prop| {
                    if let minimq::Property::UserProperty(header, code) = prop {
                        Some((header.0, code.0))
                    } else {
                        None
                    }
                })
                .unwrap();
            assert!(header == "code");
            assert!(code == "Ok");
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

    let mut buffer = [0u8; 1024];
    let stack = std_embedded_nal::Stack;
    let localhost = embedded_nal::IpAddr::V4(embedded_nal::Ipv4Addr::new(127, 0, 0, 1));
    let mqtt: minimq::Minimq<'_, _, _, minimq::broker::IpBroker> = minimq::Minimq::new(
        stack,
        StandardClock::default(),
        minimq::ConfigBuilder::new(localhost.into(), &mut buffer).keepalive_interval(60),
    );

    // Construct a settings configuration interface.
    let mut interface: minireq::Minireq<_, _, minimq::broker::IpBroker> =
        minireq::Minireq::new("minireq/integration/device", mqtt).unwrap();

    interface.subscribe("test").unwrap();

    // Update the client until the exit
    let mut should_exit = false;

    while !should_exit {
        interface
            .poll(|topic, _data, _buffer| match topic {
                "test" => {
                    should_exit = true;
                    Ok(0)
                }
                _ => Err("Unknown handler"),
            })
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}
