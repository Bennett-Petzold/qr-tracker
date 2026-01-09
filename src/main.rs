use std::{
    cell::{LazyCell, OnceCell},
    collections::HashMap,
    fmt::Write,
    sync::{mpsc, Arc, LazyLock, Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Local};
use dioxus::{
    desktop::{tao::platform::unix::WindowBuilderExtUnix, WindowBuilder},
    prelude::*,
};
use nokhwa::utils::Resolution;

use crate::{sqlite::BackingDatabase, video::video_routine};

/// Arbitrary buffer length to allow QR processing to catch up with QR input.
const QR_BUFFER_SIZE: usize = 128;
const BACKING_DATABASE_FILE: &str = "gearcats-qr-tracker.db";
const MIN_SCAN_SPACING_SECS: i64 = 5;

pub const VIDEO_SOCKET: &str = "localhost:2343";
pub const VIDEO_SOCKET_HTTP: &str = const_str::concat!("http://", VIDEO_SOCKET);

static MAIN_CSS: Asset = asset!("/assets/main.css");

mod atomic_buf;
mod sqlite;
mod video;

// Populated with all available camera resolutions on startup.
pub static CAMERA_RESOLUTION_LIST: OnceLock<Box<[Resolution]>> = OnceLock::new();

#[derive(Clone)]
struct VideoChannels {
    pub qr_reads_rx: async_channel::Receiver<String>,
    pub camera_resolution_select_tx: async_channel::Sender<Resolution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum QrType {
    Mentor,
    Student,
    Guest,
}

fn main() {
    let (qr_reads_tx, qr_reads_rx) = async_channel::bounded(QR_BUFFER_SIZE);
    let (camera_resolution_select_tx, camera_resolution_select_rx) = async_channel::bounded(1);
    thread::spawn(move || video_routine(qr_reads_tx, camera_resolution_select_rx));

    let video_channels = VideoChannels {
        qr_reads_rx,
        camera_resolution_select_tx,
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
    let backing_db = use_hook(|| {
        Arc::new(RwLock::new(BackingDatabase::new(Some(
            BACKING_DATABASE_FILE,
        ))))
    });
    let carryover_present =
        use_hook(|| backing_db.read().unwrap().get_present().into_boxed_slice());
    let known_mentor_list =
        use_hook(|| backing_db.read().unwrap().get_mentors().into_boxed_slice());
    let known_student_list =
        use_hook(|| backing_db.read().unwrap().get_students().into_boxed_slice());

    let mut mentor_string = use_signal(|| {
        carryover_present
            .iter()
            .filter(|(name, _)| known_mentor_list.contains(name));
        "".to_string()
    });
    let mut student_string = use_signal(|| "".to_string());
    let mut guest_string = use_signal(|| "".to_string());
    let mut process_change = use_signal(|| "".to_string());

    let camera_resolution_list = use_hook(|| CAMERA_RESOLUTION_LIST.wait());

    let VideoChannels {
        qr_reads_rx,
        camera_resolution_select_tx,
    } = use_context();
    let camera_resolution_select_tx_reset = camera_resolution_select_tx.clone();

    use_hook(|| {
        spawn(async move {
            let mut mentor_list = Vec::new();
            let mut student_list = Vec::new();
            let mut guest_list = Vec::new();
            let mut total_list = HashMap::new();

            loop {
                let next_qr_read = qr_reads_rx.recv().await.unwrap();
                let time = Local::now();

                // Prevent repeated QR scans.
                let previous_time = total_list.get(&next_qr_read).copied();
                total_list.insert(next_qr_read.clone(), time);
                if let Some(previous_time) = previous_time {
                    if (time - previous_time).num_seconds() < MIN_SCAN_SPACING_SECS {
                        continue;
                    }
                }

                let mut list_update = |list: &mut Vec<(String, DateTime<Local>)>,
                                       mut dest: Signal<String>,
                                       qr_name: &String| {
                    if let Some(existing_idx) = list.iter().position(|(name, _)| name == qr_name) {
                        process_change.set(format!("REMOVED {qr_name}"));
                        list.remove(existing_idx);
                    } else {
                        process_change.set(format!("ADDED {qr_name}"));
                        list.push((qr_name.clone(), time));
                    }
                    dest.set(format_evenly(list))
                };

                if known_mentor_list.contains(&next_qr_read) {
                    list_update(&mut mentor_list, mentor_string, &next_qr_read);
                } else if known_student_list.contains(&next_qr_read) {
                    list_update(&mut student_list, student_string, &next_qr_read);
                } else if next_qr_read.starts_with("Guest") {
                    list_update(&mut guest_list, guest_string, &next_qr_read);
                } else {
                    list_update(&mut guest_list, guest_string, &next_qr_read);
                    //process_change.set(format!("REJECTED {next_qr_read}"));
                    continue;
                };

                backing_db
                    .write()
                    .unwrap()
                    .add_scan(next_qr_read.as_str(), time);
            }
        })
    });

    use_resource(move || async move {
        if !process_change.is_empty() {
            tokio::time::sleep(Duration::from_mins(1)).await;
            process_change.set("".to_string());
        }
    });

    let mut resolution_select = use_signal(|| "Change Resolution");

    rsx! {
        document::Stylesheet { href: MAIN_CSS }

        div {
            class: "centered_horizontally",
            h1 { "Attendance Tracker" }
            hr {}
            h3 { color: "blue", "{process_change}" }
        }

        div {
            class: "split left",
            div {
                class: "centered",
                img { src: VIDEO_SOCKET_HTTP }
                br {}
                button {
                    onclick: move |_| {
                        resolution_select.set("Change Resolution");

                        let tx = camera_resolution_select_tx_reset.clone();
                        let resolution = camera_resolution_list.first().copied();
                        async move { if let Some(resolution) = resolution {
                            tx.send(resolution).await.unwrap()
                        }}
                    },
                    "Minimize Resolution"
                }
                select {
                    onchange: move |e| {
                        let tx = camera_resolution_select_tx.clone();
                        let selected = e.value();
                        let selected = selected.trim();
                        let resolution = camera_resolution_list.iter().find(|entry| selected == format!("{entry}").trim()).copied();
                        async move { if let Some(resolution) = resolution {
                            tx.send(resolution).await.unwrap()
                        }}
                    },
                    value: "{resolution_select}",
                    option { disabled: true, "Change Resolution" }
                    for resolution in camera_resolution_list {
                        option { "{resolution}" }
                    }
                }
            }
        }

        div {
            class: "split right",

            div {
                class: "centered",
                hr {}
                h2 { "Mentors" }
                hr {}
                pre { "{mentor_string}" }

                hr {}
                h2 { "Students" }
                hr {}
                pre { "{student_string}" }

                hr {}
                h2 { "Guests" }
                hr {}
                pre { "{guest_string}" }
            }
        }
    }
}
