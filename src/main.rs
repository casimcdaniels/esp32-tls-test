#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use esp32c3_hal as hal;

use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, Stack, StackResources};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_mbedtls::X509;
use esp_mbedtls::{asynch::Session, set_debug, Certificates, Mode, TlsVersion};
use esp_println::{print, println};
use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent,
    WifiStaDevice, WifiState,
};
use esp_wifi::{initialize, EspWifiInitFor};
use hal::clock::ClockControl;
use hal::Rng;
use hal::{embassy, peripherals::Peripherals, prelude::*, rsa::Rsa, timer::TimerGroup};
use heapless::Vec;
use rust_mqtt::client::client::MqttClient;
use rust_mqtt::client::client_config::ClientConfig;
use rust_mqtt::packet::v5::reason_codes::ReasonCode;
use rust_mqtt::utils::rng_generator::CountingRng;
use smoltcp::wire::DnsQueryType;
use static_cell::make_static;

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");
const BROKER_URL: &str = env!("BROKER_URL");
const PORT: &str = env!("PORT");
const CLIENT_1_ID: &str = env!("CLIENT_1_ID");
const DEVICE_1_USERNAME: &str = env!("DEVICE_1_USERNAME");
const DEVICE_1_PASSWORD: &str = env!("DEVICE_1_PASSWORD");

#[main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    // Configure terminal: enable newline conversion
    print!("\x1b[20h");

    let peripherals = Peripherals::take();

    let system = peripherals.SYSTEM.split();
    let clocks = ClockControl::max(system.clock_control).freeze();

    let rng = Rng::new(peripherals.RNG);
    let mut rsa = Rsa::new(peripherals.RSA);

    let timer = hal::systimer::SystemTimer::new(peripherals.SYSTIMER).alarm0;
    let init = initialize(
        EspWifiInitFor::Wifi,
        timer,
        rng,
        system.radio_clock_control,
        &clocks,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    let timer_group0 = TimerGroup::new(peripherals.TIMG0, &clocks);
    embassy::init(&clocks, timer_group0);

    let config = Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*make_static!(Stack::new(
        wifi_interface,
        config,
        make_static!(StackResources::<3>::new()),
        seed
    ));

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_millis(1_000)).await;

        let mut socket = TcpSocket::new(&stack, &mut rx_buffer, &mut tx_buffer);

        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

        let address = match stack
            .dns_query(BROKER_URL, DnsQueryType::A)
            .await
            .map(|a| a[0])
        {
            Ok(address) => address,
            Err(e) => {
                println!("DNS lookup error: {e:?}");
                continue;
            }
        };

        let port: u16 = PORT.parse().expect("Failed to parse PORT as u16");
        let remote_endpoint = (address, port);

        println!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected!");

    
        set_debug(0);

        let tls: Session<_, 4096> = Session::new(
            &mut socket,
            BROKER_URL,
            Mode::Client,
            TlsVersion::Tls1_2,
            Certificates {
                ca_chain: X509::pem(concat!(include_str!("../cert.pem"), "\0").as_bytes()).ok(),
                ..Default::default()
            },
            Some(&mut rsa),
        )
        .unwrap();

        println!("Start TLS connect");
        let mut tls = tls.connect().await.unwrap();

        println!("TLS connection established");

        // MQTT Connection Setup
        let mut config = ClientConfig::new(
            rust_mqtt::client::client_config::MqttVersion::MQTTv5,
            CountingRng(20000),
        );
        config.add_max_subscribe_qos(rust_mqtt::packet::v5::publish_packet::QualityOfService::QoS2);
        config.add_client_id(CLIENT_1_ID.try_into().unwrap());
        config.add_username(DEVICE_1_USERNAME.try_into().unwrap());
        config.add_password(DEVICE_1_PASSWORD.try_into().unwrap());
        config.max_packet_size = 4096;
        let mut recv_buffer = [0; 4096];
        let mut write_buffer = [0; 4096];

        println!("Attempting MQTT connection");
        let mut client = MqttClient::<_, 5, _>::new(
            &mut tls,
            &mut write_buffer,
            4096,
            &mut recv_buffer,
            4096,
            config,
        );

        match client.connect_to_broker().await {
            Ok(()) => {
                println!("Connected to broker");
            }
            Err(mqtt_error) => match mqtt_error {
                ReasonCode::NetworkError => {
                    println!(
                        "MQTT Network Error - Broker Connect Failure: {:?}",
                        mqtt_error
                    );
                    continue;
                }
                _ => {
                    println!("Other MQTT Error: {:?}", mqtt_error);
                    continue;
                }
            },
        }

        let mut topics_to_subscribe: Vec<&str, 5> = Vec::new();
        topics_to_subscribe.push("topicfeed").unwrap();

        let subscription_result = client.subscribe_to_topics(&topics_to_subscribe).await;
        match subscription_result {
            Ok(_) => println!("Successfully subscribed to topics"),
            Err(e) => {
                println!("Failed to subscribe: {:?}", e);
            }
        };

        // Receieve messages from MQTT broker
        loop {
            match client.receive_message().await {
                Ok((topic, message)) => {
                    let message_str = core::str::from_utf8(message).unwrap_or("<Invalid UTF-8>");
                    println!("Received message on topic {:#?}: {:#?}", topic, message_str);
                }
                Err(mqtt_error) => match mqtt_error {
                    ReasonCode::NetworkError => {
                        println!("MQTT Network Error - Client Messsage Receive Error");
                        // Consider retrying or reconnecting here
                    }

                    _ => {
                        println!("Other MQTT Error: {:?}", mqtt_error);
                        // Handle other errors
                    }
                },
            }

            Timer::after(Duration::from_millis(10000)).await;
        }
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());

    loop {
        match esp_wifi::wifi::get_wifi_state() {
            WifiState::StaConnected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                auth_method: esp_wifi::wifi::AuthMethod::None,
                ..Default::default()
            });

            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}
