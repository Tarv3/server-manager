#![feature(duration_constructors)]
use std::{
    io::{BufWriter, Write},
    process::{ChildStdin, Command, ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use crossbeam_channel::{select, Receiver, Sender};
use serde::Deserialize;

#[derive(Clone, Deserialize)]
pub enum MessageOutput {
    Text(String),
    Time,
}

#[derive(Clone, Deserialize)]
pub struct Config {
    pub command: String,
    pub magic_word: String,
    pub server_stop: String,
    #[serde(default = "default_server_wait")]
    pub kill_timeout: f64,
    pub server_timer_message: Option<Vec<MessageOutput>>,
}

fn default_server_wait() -> f64 {
    10.0
}

pub enum Event {
    SendToProcess(String),
    StartShutdown(std::time::Duration),
    StartStop(std::time::Duration),
}

#[derive(Debug)]
pub struct CommandFinished(std::io::Result<ExitStatus>);

#[derive(Debug)]
pub enum ExternalEvent {
    SendToProcess(String),
    Stdin(String),
    Stop,
    Shutdown,
    Abort,
}

pub struct Cancel;

pub struct ProcessPipe {
    pipe: BufWriter<ChildStdin>,
}

impl ProcessPipe {
    pub fn write_str(&mut self, input: &str) {
        self.pipe.write_all(input.as_bytes()).ok();
        self.pipe.write_all("\n".as_bytes()).ok();
        self.pipe.flush().ok();
    }
}

pub enum State {
    ShuttingDown { sender: Sender<Cancel> },
    ConfirmShutdown { duration: std::time::Duration },
    ConfirmStop { duration: std::time::Duration },
    Waiting,
}

impl State {
    fn handle_input(
        &mut self,
        config: &Arc<Config>,
        input: String,
        sender: &Sender<ExternalEvent>,
        writer: &mut ProcessPipe,
    ) {
        match self {
            State::ShuttingDown { sender, .. } => {
                if Self::parse_cancel(input) {
                    sender.send(Cancel).ok();
                }
            }
            State::ConfirmShutdown { duration } => {
                if Self::parse_confirm(&input) {
                    let (cancel, cancel_recv) = crossbeam_channel::bounded(0);
                    let sender = sender.clone();
                    let duration = *duration;
                    let config = config.clone();
                    std::thread::spawn(move || {
                        Self::spawn_countdown(
                            &config,
                            duration,
                            ExternalEvent::Shutdown,
                            sender,
                            cancel_recv,
                        )
                    });

                    *self = State::ShuttingDown { sender: cancel };
                }
            }
            State::ConfirmStop { duration } => {
                if Self::parse_confirm(&input) {
                    let (cancel, cancel_recv) = crossbeam_channel::bounded(0);
                    let sender = sender.clone();
                    let duration = *duration;
                    let config = config.clone();
                    std::thread::spawn(move || {
                        Self::spawn_countdown(
                            &config,
                            duration,
                            ExternalEvent::Stop,
                            sender,
                            cancel_recv,
                        )
                    });

                    *self = State::ShuttingDown { sender: cancel };
                }
            }
            State::Waiting => match Self::parse_waiting_command(&config.magic_word, input) {
                Some(Event::SendToProcess(line)) => {
                    writer.write_str(&line);
                }
                Some(Event::StartShutdown(duration)) => {
                    println!("Are you sure you want to shutdown? (y/n)");
                    *self = State::ConfirmShutdown { duration };
                }
                Some(Event::StartStop(duration)) => {
                    println!("Are you sure you want to stop? (y/n)");
                    *self = State::ConfirmStop { duration };
                }
                None => {}
            },
        }
    }

    fn spawn_countdown(
        config: &Config,
        duration: std::time::Duration,
        event: ExternalEvent,
        sender: Sender<ExternalEvent>,
        recv: Receiver<Cancel>,
    ) {
        let time_string = |time: std::time::Duration| -> String {
            let seconds = time.as_secs_f64();
            let minutes = (seconds / 60.0).floor();
            let hours = (minutes / 60.0).floor();
            let seconds = seconds % 60.0;

            if hours >= 1.0 {
                return format!("{hours:.0} hour(s)");
            }
            else if minutes >= 1.0 {
                return format!("{minutes:.0} minute(s)");
            }
            else {
                return format!("{seconds:.0} second(s)");
            }
        };

        let print_message = |time: std::time::Duration| -> String {
            let mut output = String::new();
            let time_string = time_string(time);
            for value in config.server_timer_message.as_ref().unwrap().iter() {
                match value {
                    MessageOutput::Text(txt) => output.push_str(txt),
                    MessageOutput::Time => output.push_str(&time_string),
                }
            }

            output
        };

        let mut remaining = duration;

        let sleep_times = [
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
            std::time::Duration::from_mins(1),
        ];

        loop {
            sender.send(ExternalEvent::SendToProcess(print_message(remaining))).unwrap();
            let now = std::time::Instant::now();
            let mut sleep = sleep_times[0];

            for time in sleep_times {
                if time > remaining {
                    break;
                }

                if remaining - time < time {
                    break;
                }

                sleep = time;
            }
            let sleep = sleep.min(remaining);

            select! {
                recv(recv) -> _ => return,
                default(sleep) => {}
            }

            let elapsed = now.elapsed();
            if elapsed >= remaining {
                break;
            }

            remaining -= elapsed;
        }

        sender.send(event).unwrap();
    }

    fn parse_waiting_command(magic_word: &str, line: String) -> Option<Event> {
        let mut words = line.split_ascii_whitespace();

        match words.next() {
            Some(x) if x != magic_word => {
                return Some(Event::SendToProcess(line));
            }
            _ => {}
        }

        match words.next() {
            Some("stop") => {
                if let Some(duration) = Self::parse_duration(words) {
                    return Some(Event::StartStop(duration));
                }
            }
            Some("shutdown") => {
                if let Some(duration) = Self::parse_duration(words) {
                    return Some(Event::StartShutdown(duration));
                }
            }
            _ => {}
        }

        println!("Invalid command: {line}");
        None
    }

    fn parse_duration<'a>(mut iter: impl Iterator<Item = &'a str>) -> Option<std::time::Duration> {
        let mut seconds = 0.0;
        let mut minutes = 0.0;
        let mut hours = 0.0;

        while let Some(value) = iter.next() {
            let value = value.parse::<f64>().ok()?;

            match iter.next()? {
                "hours" | "hour" => hours = value,
                "mins" | "min" => minutes = value,
                "secs" | "sec" => seconds = value,
                _ => return None,
            }
        }

        let duration = Duration::from_secs_f64(seconds + minutes * 60.0 + hours * 60.0 * 60.0);
        Some(duration)
    }

    fn parse_cancel(line: String) -> bool {
        match line.trim().to_lowercase().as_str() {
            "cancel" => {
                return true;
            }
            _ => {}
        }
        println!("Invalid command: {line}");
        false
    }

    fn parse_confirm(line: &str) -> bool {
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" => true,
            "n" | "no" => false,
            e => {
                println!("Unknown response: {e}, assuming no");
                false
            }
        }
    }
}

pub struct Kill;

fn launch_command(
    command: String,
    args: Vec<String>,
    sender: Sender<CommandFinished>,
    recv: Receiver<Kill>,
    kill_timeout: std::time::Duration,
) -> ProcessPipe {
    println!("Spawing process: {command}");
    let mut process = Command::new(command)
        .args(args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::piped())
        .spawn()
        .expect("Failed to launch");

    let input = std::io::BufWriter::new(process.stdin.take().unwrap());

    std::thread::spawn(move || {
        let check_interval = std::time::Duration::from_secs_f64(1.0);

        let result = loop {
            select! {
                recv(recv) -> _ => {
                    let Some(status) = kill_process(kill_timeout, check_interval, &mut process)
                    else {
                        continue;
                    };

                    break status;
                },
                default(std::time::Duration::from_secs_f64(1.0)) => {
                    match process.try_wait() {
                        Ok(Some(status)) => {
                            break Ok(status);

                        }
                        Err(e) => {
                            break Err(e);
                        }
                        Ok(None) => {}
                    }
                }
            }
        };

        sender.send(CommandFinished(result)).expect("Failed to send command finished");
    });

    ProcessPipe { pipe: input }
}

fn kill_process(
    kill_timeout: Duration,
    check_interval: Duration,
    process: &mut std::process::Child,
) -> Option<std::io::Result<ExitStatus>> {
    let now = std::time::Instant::now();

    while now.elapsed() < kill_timeout {
        std::thread::sleep(check_interval.min(kill_timeout - now.elapsed()));

        match process.try_wait() {
            Ok(Some(status)) => return Some(Ok(status)),
            Err(e) => return Some(Err(e)),
            _ => {}
        }
    }

    println!("Killing");
    process.kill().unwrap();

    None
}

fn read_stdin(sender: Sender<ExternalEvent>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();

        for line in stdin.lines() {
            let line = match line {
                Ok(line) => line,
                Err(e) => {
                    println!("Stopping program. Something terrible happened {e}");
                    sender.send(ExternalEvent::Stop).unwrap();
                    return;
                }
            };
            sender.send(ExternalEvent::Stdin(line)).unwrap();
        }
    });
}

fn print_status(status: std::io::Result<ExitStatus>) {
    match status {
        Ok(status) => {
            println!("Finished: {}", status.success());
        }
        Err(e) => println!("Exit Error: {e}"),
    }
}

fn main() {
    let file = std::io::BufReader::new(std::fs::File::open("config.ron").unwrap());
    let config: Config = ron::de::from_reader(file).expect("Failed to read config");
    let config = Arc::new(config);

    let (external_send, external_recv) = crossbeam_channel::unbounded();
    let (finished_send, finished_recv) = crossbeam_channel::bounded(0);
    let (kill_send, kill_recv) = crossbeam_channel::bounded(1);
    let mut state = State::Waiting;

    let input_args = config.command.split_ascii_whitespace().collect::<Vec<_>>();
    let command = input_args[0].to_string();
    let args = input_args.iter().skip(1).map(|value| value.to_string()).collect();
    let kill_timeout = std::time::Duration::from_secs_f64(config.kill_timeout);

    let mut writer = launch_command(command, args, finished_send.clone(), kill_recv, kill_timeout);
    read_stdin(external_send.clone());

    let mut sent_stop = false;
    let ctrlc_sender = external_send.clone();
    ctrlc::set_handler(move || {
        println!("Got ctrlc: {sent_stop}");
        match sent_stop {
            true => {
                ctrlc_sender.send(ExternalEvent::Abort).ok();
            }
            false => {
                sent_stop = true;
                ctrlc_sender.send(ExternalEvent::Stop).unwrap();
            }
        }
    })
    .unwrap();

    let do_shutdown = loop {
        select! {
            recv(external_recv) -> event => {
                let event = event.unwrap();
                match event {
                    ExternalEvent::SendToProcess(text) => {
                        writer.write_str(&text)
                    }
                    ExternalEvent::Stdin(input) => {
                        state.handle_input(&config, input, &external_send, &mut writer);
                    }
                    ExternalEvent::Stop => {
                        writer.write_str(&config.server_stop);
                        kill_send.send(Kill).ok();
                        break false;
                    }
                    ExternalEvent::Shutdown => {
                        writer.write_str(&config.server_stop);
                        kill_send.send(Kill).ok();
                        break true;
                    }
                    ExternalEvent::Abort => {
                        panic!("Aborting");
                    }
                }
            }
            recv(finished_recv) -> event => {
                let event = event.unwrap();
                print_status(event.0);
                return;
            }
        }
    };

    println!("Waiting for finished");
    let result = finished_recv
        .recv_timeout(std::time::Duration::from_secs_f64(10.0))
        .expect("Failed to receive finished");
    print_status(result.0);

    if do_shutdown {
        match system_shutdown::shutdown() {
            Ok(_) => println!("Shutting down, bye!"),
            Err(error) => eprintln!("Failed to shut down: {}", error),
        }
    }
}
