#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

mod device;
mod light;
mod response;
mod server;
mod state;

use alloc::boxed::Box;

use ascot::route::{LightOffRoute, LightOnRoute, Route};

use log::{error, info};

use embassy_executor::Spawner;
use embassy_net::{Config, DhcpConfig, Runner, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;

use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::rng::Rng;
use esp_hal::timer::systimer::SystemTimer;
use esp_hal::timer::timg::TimerGroup;

use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiState,
};
use esp_wifi::EspWifiController;

use crate::light::Light;
use crate::response::{EmptyResponse, IntoResponse, Response, TextResponse};
use crate::server::Server;

use esp_backtrace as _;

const MAX_HEAP_SIZE: usize = 64 * 1024;
const MILLISECONDS_TO_WAIT: u64 = 100;
const SECONDS_TO_WAIT_FOR_RECONNECTION: u64 = 5;

// Socket buffer size.
const TX_SIZE: usize = 2048;
// Server buffer size.
const RX_SIZE: usize = 4096;
// Maximum number of allowed headers in a request.
const MAXIMUM_HEADERS_COUNT: usize = 32;
// Timeout.
const TIMEOUT: u32 = 15 * 1000;

// Signal which notifies the led change of state.
static NOTIFY_LED: Signal<CriticalSectionRawMutex, LedInput> = Signal::new();

macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

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

async fn turn_light_on() -> Response {
    // Notify led to turn led on.
    NOTIFY_LED.signal(LedInput::On);

    log::info!("Led turned on through GET route!");

    // Wait for some time before starting the loop again.
    Timer::after_millis(MILLISECONDS_TO_WAIT).await;

    // Returns an empty response.
    TextResponse::new("Light on").into_response()
}

async fn turn_light_off() -> Response {
    // Notify led to turn led off.
    NOTIFY_LED.signal(LedInput::Off);

    log::info!("Led turned off through GET route!");

    // Wait for some time before starting the loop again.
    Timer::after_millis(MILLISECONDS_TO_WAIT).await;

    // Returns an empty response.
    TextResponse::new("Light off").into_response()
}

#[cfg(feature = "state")]
struct RequestCounter(&'static core::sync::atomic::AtomicU32);

#[cfg(feature = "state")]
impl crate::state::ValueFromRef for RequestCounter {
    fn value_from_ref(&self) -> Self {
        Self(self.0)
    }
}

#[cfg(feature = "state")]
async fn stateful_toggle(
    crate::state::State(RequestCounter(request_counter)): crate::state::State<RequestCounter>,
) -> Response {
    let old_value = request_counter.load(core::sync::atomic::Ordering::Relaxed);
    request_counter.store(old_value + 1, core::sync::atomic::Ordering::Relaxed);
    log::info!("Request number: {request_counter:?}");
    EmptyResponse::ok().into_response()
}

async fn run(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: MAX_HEAP_SIZE);

    let timer0 = SystemTimer::new(peripherals.SYSTIMER);
    esp_hal_embassy::init(timer0.alarm0);

    info!("Embassy initialized!");

    let rng = esp_hal::rng::Rng::new(peripherals.RNG);
    let timer1 = TimerGroup::new(peripherals.TIMG0);

    let wifi_init = &*mk_static!(
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
    let (stack, runner) = create_stack::<9>(rng, interfaces.sta);

    spawner.spawn(connect(wifi_controller)).unwrap();
    spawner.spawn(net_task(runner)).unwrap();

    // Input button
    let button = Input::new(
        peripherals.GPIO9,
        InputConfig::default().with_pull(Pull::Up),
    );

    // Output led.
    let led = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    spawner.spawn(press_button(button)).unwrap();
    spawner.spawn(change_led(led)).unwrap();

    #[cfg(not(feature = "state"))]
    let device = Light::new(&interfaces.ap)
        .turn_light_on_stateless(
            LightOnRoute::put("On").description("Turn light on."),
            turn_light_on,
        )
        .turn_light_off_stateless(
            LightOffRoute::put("Off").description("Turn light off."),
            turn_light_off,
        )
        .stateless_route(
            Route::get("Toggle", "/toggle").description("Toggle."),
            || async move { EmptyResponse::ok().into_response() },
        )
        .build();

    #[cfg(feature = "state")]
    let request_counter = RequestCounter(mk_static!(
        core::sync::atomic::AtomicU32,
        core::sync::atomic::AtomicU32::new(0)
    ));
    #[cfg(feature = "state")]
    let device = Light::with_state(&interfaces.ap, request_counter)
        .turn_light_on_stateful(
            LightOnRoute::put("On").description("Turn light on."),
            |crate::state::State(RequestCounter(request_counter)): crate::state::State<
                RequestCounter,
            >| async move { turn_light_on().await },
        )
        .turn_light_off_stateful(
            LightOffRoute::put("Off").description("Turn light off."),
            |crate::state::State(RequestCounter(request_counter)): crate::state::State<
                RequestCounter,
            >| async move { turn_light_off().await },
        )
        .stateful_route(
            Route::get("Toggle", "/toggle").description("Toggle."),
            stateful_toggle,
        )
        .build();

    Server::<TX_SIZE, RX_SIZE, MAXIMUM_HEADERS_COUNT, TIMEOUT, _>::new(device)
        .run(stack)
        .await
        .expect("Failed to run a server");
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    run(spawner).await;
}
