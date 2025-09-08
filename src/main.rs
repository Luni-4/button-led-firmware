#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

mod server;

use core::net::Ipv4Addr;

use alloc::boxed::Box;

use log::{error, info};

use embassy_executor::Spawner;
use embassy_net::{Config, DhcpConfig, Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};

use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::rng::Rng;
use esp_hal::timer::systimer::SystemTimer;
use esp_hal::timer::timg::TimerGroup;

use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState,
};
use esp_wifi::EspWifiController;

use picoserve::{make_static, AppBuilder, AppRouter};

use esp_backtrace as _;

use crate::server::{run_server, AppProps};

const MAX_HEAP_SIZE: usize = 64 * 1024;
const MILLISECONDS_TO_WAIT: u64 = 100;
const SECONDS_TO_WAIT_FOR_RECONNECTION: u64 = 5;

// Signal which notifies the led change of state.
static NOTIFY_LED: Signal<CriticalSectionRawMutex, LedInput> = Signal::new();

#[toml_cfg::toml_config]
struct DeviceConfig {
    #[default("")]
    ssid: &'static str,
    #[default("")]
    password: &'static str,
}

#[derive(Clone, Copy)]
enum LedInput {
    On,
    Off,
    Button,
}

// This creates a default app-descriptor required by the esp-idf bootloader.
// For more information see: <https://docs.espressif.com/projects/esp-idf/en/stable/esp32/api-reference/system/app_image_format.html#application-description>
esp_bootloader_esp_idf::esp_app_desc!();

#[embassy_executor::task]
pub async fn connect(mut wifi_controller: WifiController<'static>) {
    info!("Wi-Fi connection task started");
    loop {
        if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
            wifi_controller
                .wait_for_event(WifiEvent::StaDisconnected)
                .await;
            Timer::after_secs(SECONDS_TO_WAIT_FOR_RECONNECTION).await;
        }

        if !matches!(wifi_controller.is_started(), Ok(true)) {
            info!("Starting Wi-Fi...");
            wifi_controller.start_async().await.unwrap();
            info!("Wi-Fi started");
        }

        info!("Attempting to connect...");
        if let Err(e) = wifi_controller.connect_async().await {
            error!("Wi-Fi connect failed: {e:?}");
            Timer::after_secs(SECONDS_TO_WAIT_FOR_RECONNECTION).await;
        } else {
            info!("Wi-Fi connected!");
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await;
}

#[embassy_executor::task]
async fn press_button(mut button: Input<'static>) {
    loop {
        // Wait for Button Press
        button.wait_for_rising_edge().await;
        info!("Button Pressed!");

        // Notify led to change its state.
        NOTIFY_LED.signal(LedInput::Button);

        // Wait for some time before starting the loop again.
        Timer::after_millis(MILLISECONDS_TO_WAIT).await;
    }
}

// Set led to on.
fn led_on(led: &mut Output<'static>) {
    led.set_low();
    info!("Led is on!");
}

// Set led to off.
fn led_off(led: &mut Output<'static>) {
    led.set_high();
    info!("Led is off!");
}

#[embassy_executor::task]
async fn change_led(mut led: Output<'static>) {
    loop {
        // Wait for until a signal is received.
        let led_input = NOTIFY_LED.wait().await;

        match led_input {
            LedInput::On => {
                led_on(&mut led);
            }
            LedInput::Off => {
                led_off(&mut led);
            }
            LedInput::Button => {
                // Switch on or off the led.
                //
                // Check whether the led is on.
                if led.is_set_high() {
                    led_on(&mut led);
                } else {
                    led_off(&mut led);
                }
            }
        }

        // TODO: We should insert here the `embassy-events` notifier code that
        // writes the event over the network using mqtt.

        // Wait for some time before starting the loop again.
        Timer::after_millis(MILLISECONDS_TO_WAIT).await;
    }
}

fn create_stack<const SOCKET_STACK_SIZE: usize>(
    mut rng: Rng,
    wifi_interface: WifiDevice<'static>,
) -> (Stack<'static>, Runner<'static, WifiDevice<'static>>) {
    let config = Config::dhcpv4(DhcpConfig::default());
    let seed = u64::from(rng.random()) << 32 | u64::from(rng.random());

    // FIXME: We need to use `Box::leak` and then `Box::new` because
    // `make_static` does not accept **ANY** kind of generic, not even const
    // generics.
    let resources = Box::leak(Box::new(StackResources::<SOCKET_STACK_SIZE>::new()));

    let (stack, runner) = embassy_net::new(wifi_interface, config, resources, seed);

    (stack, runner)
}

async fn get_ip(stack: Stack<'_>) -> Ipv4Addr {
    info!("Waiting till the link is up...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after_millis(MILLISECONDS_TO_WAIT).await;
    }

    info!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            info!("Got IP: {}", config.address);
            return config.address.address();
        }
        Timer::after_millis(MILLISECONDS_TO_WAIT).await;
    }
}

async fn run<const WEB_TASK_POOL_SIZE: usize>(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: MAX_HEAP_SIZE);

    let timer0 = SystemTimer::new(peripherals.SYSTIMER);
    esp_hal_embassy::init(timer0.alarm0);

    info!("Embassy initialized!");

    let rng = esp_hal::rng::Rng::new(peripherals.RNG);
    let timer1 = TimerGroup::new(peripherals.TIMG0);

    let wifi_init = &*make_static!(
        EspWifiController<'static>,
        esp_wifi::init(timer1.timer0, rng).expect("Failed to initialize Wi-Fi/BLE controller")
    );

    let (mut wifi_controller, interfaces) = esp_wifi::wifi::new(wifi_init, peripherals.WIFI)
        .expect("Failed to initialize WIFI controller");

    // Retrieve device configuration
    let device_config = DEVICE_CONFIG;

    assert!(!device_config.ssid.is_empty(), "Missing Wi-Fi SSID");

    assert!(!device_config.password.is_empty(), "Missing Wi-Fi password");

    let client_config = Configuration::Client(ClientConfiguration {
        ssid: device_config.ssid.into(),
        password: device_config.password.into(),
        ..Default::default()
    });

    wifi_controller.set_configuration(&client_config).unwrap();

    // We need to pass this value in this way because it is not possible
    // to increment a const value coming from outside.
    let (stack, runner) = match WEB_TASK_POOL_SIZE.max(1) {
        1 => create_stack::<2>(rng, interfaces.sta),
        2 => create_stack::<3>(rng, interfaces.sta),
        3 => create_stack::<4>(rng, interfaces.sta),
        4 => create_stack::<5>(rng, interfaces.sta),
        5 => create_stack::<6>(rng, interfaces.sta),
        6 => create_stack::<7>(rng, interfaces.sta),
        7 => create_stack::<8>(rng, interfaces.sta),
        _ => create_stack::<9>(rng, interfaces.sta),
    };

    spawner.spawn(connect(wifi_controller)).unwrap();
    spawner.spawn(net_task(runner)).unwrap();

    let ip = get_ip(stack).await;
    info!("Got IP Address: {ip}");

    // Input button
    let button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    // Output led.
    let led = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    spawner.spawn(press_button(button)).unwrap();
    spawner.spawn(change_led(led)).unwrap();

    let app = make_static!(AppRouter<AppProps>, AppProps.build_app());

    let config = make_static!(
        picoserve::Config<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            persistent_start_read_request: Some(Duration::from_secs(1)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );

    run_server::<WEB_TASK_POOL_SIZE>(spawner, stack, app, config).await;
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    run::<8>(spawner).await;
}
