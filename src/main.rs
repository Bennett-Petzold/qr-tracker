use std::{
    collections::HashMap,
    fmt::Write,
    rc::Rc,
    sync::{OnceLock, RwLock},
    thread,
    time::Duration,
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

        writeln!(out, "\t{}", time.format("%m-%d-%Y %H:%M:%S %p")).unwrap();
    }

    // Remove trailing newline
    out.pop();

    out
}

#[component]
fn app() -> Element {
    let backing_db = use_hook(|| {
        Rc::new(RwLock::new(BackingDatabase::new(Some(
            BACKING_DATABASE_FILE,
        ))))
    });
    let backing_db_process_change = backing_db.clone();
    let backing_db_select = backing_db.clone();
    let backing_db_select_reset = backing_db.clone();

    let mut mentor_string = use_signal(|| "".to_string());
    let mut student_string = use_signal(|| "".to_string());
    let mut guest_string = use_signal(|| "".to_string());
    let mut process_change = use_signal(|| "".to_string());

    let camera_resolution_list = use_hook(|| CAMERA_RESOLUTION_LIST.wait());

    let VideoChannels {
        qr_reads_rx,
        camera_resolution_select_tx,
    } = use_context();
    let camera_resolution_select_tx_reset = camera_resolution_select_tx.clone();

    // Set camera resolution with any existing selection.
    use_hook(|| {
        if let Some(resolution) = backing_db.read().unwrap().get_resolution() {
            camera_resolution_select_tx
                .send_blocking(resolution)
                .unwrap();
        }
    });

    // Updates attendance lists.
    use_hook(|| {
        spawn(async move {
            let carryover_present = backing_db.read().unwrap().get_present();
            let known_mentors = backing_db.read().unwrap().get_mentors().into_boxed_slice();
            let known_students = backing_db.read().unwrap().get_students().into_boxed_slice();

            let mut mentor_list: Vec<_> = carryover_present
                .iter()
                .filter(|(name, _)| known_mentors.contains(name))
                .map(|(name, time)| (name.clone(), *time))
                .collect();
            mentor_string.set(format_evenly(&mentor_list));

            let mut student_list: Vec<_> = carryover_present
                .iter()
                .filter(|(name, _)| known_students.contains(name))
                .map(|(name, time)| (name.clone(), *time))
                .collect();
            student_string.set(format_evenly(&student_list));

            let mut guest_list: Vec<_> = carryover_present
                .iter()
                .filter(|(name, _)| name.starts_with("Guest"))
                .map(|(name, time)| (name.clone(), *time))
                .collect();
            guest_string.set(format_evenly(&guest_list));

            let mut total_list: HashMap<_, _> = carryover_present.into_iter().collect();

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

                if known_mentors.contains(&next_qr_read) {
                    list_update(&mut mentor_list, mentor_string, &next_qr_read);
                } else if known_students.contains(&next_qr_read) {
                    list_update(&mut student_list, student_string, &next_qr_read);
                } else if next_qr_read.starts_with("Guest") {
                    list_update(&mut guest_list, guest_string, &next_qr_read);
                } else {
                    process_change.set(format!("REJECTED {next_qr_read}"));
                    continue;
                };

                backing_db
                    .write()
                    .unwrap()
                    .add_scan(next_qr_read.as_str(), time);
            }
        })
    });

    use_resource(move || {
        let backing_db_process_change = backing_db_process_change.clone();
        async move {
            if !process_change.is_empty() {
                tokio::time::sleep(Duration::from_mins(1)).await;
                process_change.set("".to_string());
                backing_db_process_change.read().unwrap().checkpoint();
            }
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
                        let resolution = camera_resolution_list
                            .first()
                            .copied();

                        if let Some(resolution) = &resolution {
                            backing_db_select_reset
                                .write()
                                .unwrap()
                                .set_resolution(*resolution);
                        }

                        let tx = camera_resolution_select_tx_reset.clone();
                        async move { if let Some(resolution) = resolution {
                            tx.send(resolution).await.unwrap();
                        }}
                    },
                    "Minimize Resolution"
                }
                select {
                    onchange: move |e| {
                        let tx = camera_resolution_select_tx.clone();
                        let selected = e.value();
                        let selected = selected.trim();
                        let resolution = camera_resolution_list
                            .iter()
                            .find(|entry| selected == format!("{entry}").trim())
                            .copied();

                        if let Some(resolution) = &resolution {
                            backing_db_select
                                .write()
                                .unwrap()
                                .set_resolution(*resolution);
                        }

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
