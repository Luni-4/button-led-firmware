use embassy_executor::Spawner;

use embassy_net::Stack;
use embassy_time::{Duration, Timer};

use picoserve::{
    listen_and_serve,
    routing::{get, PathRouter, Router},
    AppBuilder, AppRouter, Config,
};

use crate::{LedInput, MILLISECONDS_TO_WAIT, NOTIFY_LED};

macro_rules! web_task {
    ($pool_size_ident:ident, $pool_size_value:tt) => {
        #[embassy_executor::task(pool_size = $pool_size_value)]
        async fn $pool_size_ident(
            id: usize,
            stack: Stack<'static>,
            app: &'static AppRouter<AppProps>,
            config: &'static Config<Duration>,
        ) {
            web_task(id, stack, app, config).await
        }
    };
}

pub(crate) struct AppProps;

impl AppBuilder for AppProps {
    type PathRouter = impl PathRouter;

    fn build_app(self) -> Router<Self::PathRouter> {
        Router::new()
            .route(
                "/on",
                get(|| async move {
                    // Notify led to turn led on.
                    NOTIFY_LED.signal(LedInput::On);

                    log::info!("Led turned on through GET route!");

                    // Wait for some time before starting the loop again.
                    Timer::after_millis(MILLISECONDS_TO_WAIT).await;
                }),
            )
            .route(
                "/off",
                get(|| async move {
                    // Notify led to turn led off.
                    NOTIFY_LED.signal(LedInput::Off);

                    log::info!("Led turned off through GET route!");

                    // Wait for some time before starting the loop again.
                    Timer::after_millis(MILLISECONDS_TO_WAIT).await;
                }),
            )
    }
}

pub(crate) async fn run_server<const WEB_TASK_POOL_SIZE: usize>(
    spawner: Spawner,
    stack: Stack<'static>,
    app: &'static AppRouter<AppProps>,
    config: &'static Config<Duration>,
) {
    for id in 0..WEB_TASK_POOL_SIZE.max(1) {
        match WEB_TASK_POOL_SIZE.max(1) {
            1 => {
                spawner.spawn(web_task1(id, stack, app, config)).unwrap();
            }
            2 => {
                spawner.spawn(web_task2(id, stack, app, config)).unwrap();
            }
            3 => {
                spawner.spawn(web_task3(id, stack, app, config)).unwrap();
            }
            4 => {
                spawner.spawn(web_task4(id, stack, app, config)).unwrap();
            }
            5 => {
                spawner.spawn(web_task5(id, stack, app, config)).unwrap();
            }
            6 => {
                spawner.spawn(web_task6(id, stack, app, config)).unwrap();
            }
            7 => {
                spawner.spawn(web_task7(id, stack, app, config)).unwrap();
            }
            _ => {
                spawner.spawn(web_task8(id, stack, app, config)).unwrap();
            }
        }
    }
}

#[inline]
#[allow(clippy::similar_names)]
async fn web_task(
    id: usize,
    stack: Stack<'static>,
    app: &'static AppRouter<AppProps>,
    config: &'static Config<Duration>,
) {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    listen_and_serve(
        id,
        app,
        config,
        stack,
        port,
        &mut tcp_rx_buffer,
        &mut tcp_tx_buffer,
        &mut http_buffer,
    )
    .await;
}

web_task!(web_task1, 1);
web_task!(web_task2, 2);
web_task!(web_task3, 3);
web_task!(web_task4, 4);
web_task!(web_task5, 5);
web_task!(web_task6, 6);
web_task!(web_task7, 7);
web_task!(web_task8, 8);
