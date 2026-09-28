use std::{future::Future, time::Duration};

use clap::Parser;
use clash_man::{
    api::ControllerClient,
    app::{ApiEvent, App, Command, InputOutcome},
    config::{Cli, CliCommand, ControllerSettings},
    model::{LogEntry, MemorySample, TrafficSample},
    subscription::{self, Outcome, SubscriptionError, SubscriptionSettings},
    ui,
};
use color_eyre::eyre::{Result, WrapErr};
use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use serde_json::json;
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{MissedTickBehavior, interval, sleep},
};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    let settings = ControllerSettings::resolve(&cli).wrap_err("configuration failed")?;
    let subscription = SubscriptionSettings::resolve(&cli, &settings)
        .wrap_err("subscription configuration failed")?;
    let client = ControllerClient::new(&settings).wrap_err("controller client setup failed")?;

    if let Some(CliCommand::Update { force }) = cli.command {
        return run_update(&subscription, &client, force).await;
    }

    let mut app = App::new(settings.group_test_urls.clone());
    app.set_config_source(Some(subscription.config_path.display().to_string()));
    app.subscription = subscription.load_state();
    app.subscription_interval = subscription.interval;

    let terminal = ratatui::init();
    let result = run(terminal, client, subscription, app).await;
    ratatui::restore();
    result
}

/// Headless entry point for the systemd timer; prints one line per fact for the journal.
async fn run_update(
    settings: &SubscriptionSettings,
    client: &ControllerClient,
    force: bool,
) -> Result<()> {
    match subscription::update(settings, client, force).await {
        Ok(outcome) => {
            println!("{outcome}");
            if let Some(usage) = settings.load_state().usage {
                println!("{}", usage.summary(subscription::unix_now()));
            }
            Ok(())
        }
        Err(error) => {
            eprintln!("clash-man update: {error}");
            std::process::exit(1);
        }
    }
}

async fn run(
    mut terminal: ratatui::DefaultTerminal,
    client: ControllerClient,
    subscription: SubscriptionSettings,
    mut app: App,
) -> Result<()> {
    let (event_tx, mut event_rx) = mpsc::channel(256);
    let mut tasks = JoinSet::new();
    spawn_background_tasks(&mut tasks, client.clone(), event_tx.clone());
    tasks.spawn(subscription_scheduler(
        client.clone(),
        subscription.clone(),
        event_tx.clone(),
    ));

    let mut input = EventStream::new();
    let mut redraw = interval(Duration::from_millis(100));
    redraw.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = redraw.tick() => {
                terminal.draw(|frame| ui::render(frame, &app))?;
            }
            Some(api_event) = event_rx.recv() => {
                if let Some(command) = app.apply(api_event) {
                    spawn_command(&mut tasks, client.clone(), subscription.clone(), event_tx.clone(), command);
                }
            }
            maybe_event = input.next() => match maybe_event {
                Some(Ok(Event::Key(key))) => {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                    match app.handle_key(key) {
                        InputOutcome::Continue => {}
                        InputOutcome::Quit => break,
                        InputOutcome::Run(command) => spawn_command(&mut tasks, client.clone(), subscription.clone(), event_tx.clone(), command),
                    }
                }
                Some(Ok(Event::Resize(_, _))) => {
                    terminal.draw(|frame| ui::render(frame, &app))?;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    app.apply(ApiEvent::Status(Err(format!("terminal input failed: {error}"))));
                }
                None => break,
            },
        }
    }

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

fn spawn_background_tasks(
    tasks: &mut JoinSet<()>,
    client: ControllerClient,
    sender: mpsc::Sender<ApiEvent>,
) {
    tasks.spawn(snapshot_poller(client.clone(), sender.clone()));
    tasks.spawn(connection_poller(client.clone(), sender.clone()));
    tasks.spawn(initial_metadata(client.clone(), sender.clone()));
    tasks.spawn(traffic_stream(client.clone(), sender.clone()));
    tasks.spawn(memory_stream(client.clone(), sender.clone()));
    tasks.spawn(log_stream(client, sender));
}

async fn snapshot_poller(client: ControllerClient, sender: mpsc::Sender<ApiEvent>) {
    let mut ticker = interval(Duration::from_secs(5));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let (proxies, config) = tokio::join!(client.proxies(), client.configs());
        if sender
            .send(ApiEvent::Proxies(
                proxies.map_err(|error| error.to_string()),
            ))
            .await
            .is_err()
        {
            break;
        }
        if sender
            .send(ApiEvent::Config(config.map_err(|error| error.to_string())))
            .await
            .is_err()
        {
            break;
        }
    }
}

/// Updates the subscription whenever it is due while the dashboard is open.
async fn subscription_scheduler(
    client: ControllerClient,
    settings: SubscriptionSettings,
    sender: mpsc::Sender<ApiEvent>,
) {
    let mut ticker = interval(Duration::from_secs(300));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let result = subscription::update(&settings, &client, false).await;
        if !report_subscription(&client, &settings, &sender, result, true).await {
            break;
        }
    }
}

/// Publishes an update result; `quiet` hides results that need no attention.
/// Returns false once the dashboard has gone away.
async fn report_subscription(
    client: &ControllerClient,
    settings: &SubscriptionSettings,
    sender: &mpsc::Sender<ApiEvent>,
    result: Result<Outcome, SubscriptionError>,
    quiet: bool,
) -> bool {
    let reloaded = matches!(result, Ok(Outcome::Reloaded));
    let status = match result {
        Ok(Outcome::NotDue { .. }) | Err(SubscriptionError::Busy) if quiet => None,
        Ok(outcome) => Some(Ok(outcome.to_string())),
        Err(error) => Some(Err(error.to_string())),
    };
    if let Some(status) = status
        && sender.send(ApiEvent::Status(status)).await.is_err()
    {
        return false;
    }
    if sender
        .send(ApiEvent::Subscription(settings.load_state()))
        .await
        .is_err()
    {
        return false;
    }
    if reloaded {
        let (proxies, rules) = tokio::join!(client.proxies(), client.rules());
        let _ = sender
            .send(ApiEvent::Proxies(
                proxies.map_err(|error| error.to_string()),
            ))
            .await;
        let _ = sender
            .send(ApiEvent::Rules(rules.map_err(|error| error.to_string())))
            .await;
        send_config(client, sender).await;
    }
    true
}

async fn connection_poller(client: ControllerClient, sender: mpsc::Sender<ApiEvent>) {
    let mut ticker = interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if sender
            .send(ApiEvent::Connections(
                client
                    .connections()
                    .await
                    .map_err(|error| error.to_string()),
            ))
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn initial_metadata(client: ControllerClient, sender: mpsc::Sender<ApiEvent>) {
    let (version, rules) = tokio::join!(client.version(), client.rules());
    let _ = sender
        .send(ApiEvent::Version(
            version.map_err(|error| error.to_string()),
        ))
        .await;
    let _ = sender
        .send(ApiEvent::Rules(rules.map_err(|error| error.to_string())))
        .await;
}

async fn traffic_stream(client: ControllerClient, sender: mpsc::Sender<ApiEvent>) {
    reconnecting_stream(
        "traffic",
        sender,
        move |sample_sender| {
            let client = client.clone();
            async move { client.stream_traffic(sample_sender).await }
        },
        ApiEvent::Traffic,
    )
    .await;
}

async fn memory_stream(client: ControllerClient, sender: mpsc::Sender<ApiEvent>) {
    reconnecting_stream(
        "memory",
        sender,
        move |sample_sender| {
            let client = client.clone();
            async move { client.stream_memory(sample_sender).await }
        },
        ApiEvent::Memory,
    )
    .await;
}

async fn log_stream(client: ControllerClient, sender: mpsc::Sender<ApiEvent>) {
    reconnecting_stream(
        "logs",
        sender,
        move |sample_sender| {
            let client = client.clone();
            async move { client.stream_logs(sample_sender).await }
        },
        ApiEvent::Log,
    )
    .await;
}

async fn reconnecting_stream<T, Factory, Fut, Map>(
    name: &'static str,
    sender: mpsc::Sender<ApiEvent>,
    factory: Factory,
    map: Map,
) where
    T: Send + 'static,
    Factory: Fn(mpsc::Sender<T>) -> Fut,
    Fut: Future<Output = clash_man::api::ApiResult<()>>,
    Map: Fn(T) -> ApiEvent,
{
    let mut backoff = Duration::from_millis(500);
    loop {
        let (sample_tx, mut sample_rx) = mpsc::channel(32);
        let stream = factory(sample_tx);
        tokio::pin!(stream);
        let error = loop {
            tokio::select! {
                result = &mut stream => break result.err().map(|error| error.to_string()),
                sample = sample_rx.recv() => match sample {
                    Some(sample) => {
                        backoff = Duration::from_millis(500);
                        if sender.send(map(sample)).await.is_err() {
                            return;
                        }
                    }
                    None => break Some("stream sample channel closed".into()),
                }
            }
        };
        if let Some(error) = error
            && sender
                .send(ApiEvent::StreamError {
                    stream: name,
                    error,
                })
                .await
                .is_err()
        {
            return;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

fn spawn_command(
    tasks: &mut JoinSet<()>,
    client: ControllerClient,
    subscription: SubscriptionSettings,
    sender: mpsc::Sender<ApiEvent>,
    command: Command,
) {
    tasks.spawn(async move {
        match command {
            Command::RefreshAll => {
                let (version, proxies, connections, rules, config) = tokio::join!(
                    client.version(),
                    client.proxies(),
                    client.connections(),
                    client.rules(),
                    client.configs()
                );
                let events = [
                    ApiEvent::Version(version.map_err(|error| error.to_string())),
                    ApiEvent::Proxies(proxies.map_err(|error| error.to_string())),
                    ApiEvent::Connections(connections.map_err(|error| error.to_string())),
                    ApiEvent::Rules(rules.map_err(|error| error.to_string())),
                    ApiEvent::Config(config.map_err(|error| error.to_string())),
                ];
                for event in events {
                    if sender.send(event).await.is_err() {
                        return;
                    }
                }
                let _ = sender.send(ApiEvent::Status(Ok("Refreshed".into()))).await;
            }
            Command::SelectProxy { group, proxy } => {
                let result = client
                    .select_proxy(&group, &proxy)
                    .await
                    .map(|()| format!("{group} → {proxy}"))
                    .map_err(|error| error.to_string());
                let _ = sender.send(ApiEvent::Status(result)).await;
                send_proxies(&client, &sender).await;
            }
            Command::TestGroup { group, url } => {
                let result = client
                    .group_delay(&group, &url)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                // Fresh delays first, so the "testing" badge never clears over stale numbers.
                send_proxies(&client, &sender).await;
                let _ = sender.send(ApiEvent::GroupTested { group, result }).await;
            }
            Command::CloseConnection { id } => {
                let result = client
                    .close_connection(&id)
                    .await
                    .map(|()| "Connection closed".into())
                    .map_err(|error| error.to_string());
                let _ = sender.send(ApiEvent::Status(result)).await;
                send_connections(&client, &sender).await;
            }
            Command::CloseAllConnections => {
                let result = client
                    .close_all_connections()
                    .await
                    .map(|()| "All connections closed".into())
                    .map_err(|error| error.to_string());
                let _ = sender.send(ApiEvent::Status(result)).await;
                send_connections(&client, &sender).await;
            }
            Command::SetMode(mode) => {
                let result = client
                    .patch_config(json!({ "mode": mode }))
                    .await
                    .map(|()| format!("Mode changed to {mode}"))
                    .map_err(|error| error.to_string());
                let _ = sender.send(ApiEvent::Status(result)).await;
                send_config(&client, &sender).await;
            }
            Command::UpdateSubscription => {
                let result = subscription::update(&subscription, &client, true).await;
                report_subscription(&client, &subscription, &sender, result, false).await;
            }
        }
    });
}

async fn send_proxies(client: &ControllerClient, sender: &mpsc::Sender<ApiEvent>) {
    let event = ApiEvent::Proxies(client.proxies().await.map_err(|error| error.to_string()));
    let _ = sender.send(event).await;
}

async fn send_connections(client: &ControllerClient, sender: &mpsc::Sender<ApiEvent>) {
    let event = ApiEvent::Connections(
        client
            .connections()
            .await
            .map_err(|error| error.to_string()),
    );
    let _ = sender.send(event).await;
}

async fn send_config(client: &ControllerClient, sender: &mpsc::Sender<ApiEvent>) {
    let event = ApiEvent::Config(client.configs().await.map_err(|error| error.to_string()));
    let _ = sender.send(event).await;
}

// Keep stream payload types visible to rustdoc and prevent accidental contract drift.
const _: fn(TrafficSample, MemorySample, LogEntry) = |_, _, _| {};
