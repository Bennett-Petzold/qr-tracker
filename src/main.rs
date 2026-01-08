use std::{
    fmt::Write,
    sync::{mpsc, Arc, Mutex, OnceLock},
    thread,
    time::SystemTime,
};

use chrono::{DateTime, Local};
use dioxus::{
    desktop::{tao::platform::unix::WindowBuilderExtUnix, WindowBuilder},
    prelude::*,
};
use futures::StreamExt;

use crate::video::video_routine;

/// Arbitrary buffer length to allow QR processing to catch up with QR input.
const QR_BUFFER_SIZE: usize = 128;

pub const VIDEO_SOCKET: &str = "localhost:2343";
pub const VIDEO_SOCKET_HTTP: &str = const_str::concat!("http://", VIDEO_SOCKET);

static MAIN_CSS: Asset = asset!("/assets/main.css");

mod atomic_buf;
mod video;

#[derive(Clone)]
struct VideoChannels {
    pub qr_reads_rx: Arc<Mutex<Option<futures::channel::mpsc::Receiver<String>>>>,
}

fn main() {
    let (qr_reads_tx, qr_reads_rx) = futures::channel::mpsc::channel(QR_BUFFER_SIZE);
    thread::spawn(move || video_routine(qr_reads_tx));

    let video_channels = VideoChannels {
        qr_reads_rx: Arc::new(Mutex::new(Some(qr_reads_rx))),
    };

    dioxus::LaunchBuilder::new()
        .with_cfg(desktop! {
            dioxus_desktop::Config::default()
                .with_menu(None)
                .with_window(
                WindowBuilder::new()
                    .with_fullscreen(Some(dioxus_desktop::tao::window::Fullscreen::Borderless(
                        None,
                    )))
                    .with_skip_taskbar(true)
                    ,
            )
        })
        .with_context(video_channels)
        .launch(app);
}

fn format_evenly(entries: &[(String, DateTime<Local>)]) -> String {
    let longest_name = entries
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);

    let mut out = String::new();

    for (name, time) in entries {
        out.push_str(name);
        for _ in 0..(longest_name - name.len()) {
            out.push(' ');
        }

        writeln!(out, " {}", time.format("%m-%d-%Y %H:%M:%S %p")).unwrap();
    }

    // Remove trailing newline
    out.pop();

    out
}

#[component]
fn app() -> Element {
    let mut mentors = use_signal(|| "".to_string());
    let mut students = use_signal(|| "".to_string());
    let mut guests = use_signal(|| "".to_string());
    let mut change = use_signal(|| "".to_string());

    let VideoChannels { qr_reads_rx } = use_context();

    spawn(async move {
        let qr_reads_rx = qr_reads_rx.lock().unwrap().take();
        if let Some(mut qr_reads_rx) = qr_reads_rx {
            let mut mentor_list = Vec::new();
            let mut student_list = Vec::new();
            let mut guest_list = Vec::new();

            loop {
                let next_qr_read = qr_reads_rx.next().await.unwrap();

                mentor_list.push((next_qr_read.clone(), Local::now()));
                student_list.push((next_qr_read.clone(), Local::now()));
                guest_list.push((next_qr_read.clone(), Local::now()));

                mentors.set(format_evenly(&mentor_list));
                students.set(format_evenly(&student_list));
                guests.set(format_evenly(&guest_list));

                change.set(format!("ADDED {next_qr_read}"));
                //change.set(format!("REMOVED {next_qr_read}"));
            }
        }
    });

    rsx! {
        document::Stylesheet { href: MAIN_CSS }

        div {
            class: "centered_horizontally",
            h1 { "Attendance Tracker" }
            hr {}
            h3 { color: "blue", "{change}" }
        }

        div {
            class: "split left",
            div {
                class: "centered",
                img { src: VIDEO_SOCKET_HTTP }
            }
        }

        div {
            class: "split right",

            div {
                class: "centered",
                hr {}
                h2 { "Mentors" }
                hr {}
                pre { "{mentors}" }

                hr {}
                h2 { "Students" }
                hr {}
                pre { "{students}" }

                hr {}
                h2 { "Guests" }
                hr {}
                pre { "{guests}" }
            }
        }
    }
}
