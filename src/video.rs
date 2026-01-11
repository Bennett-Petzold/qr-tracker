/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use image::{DynamicImage, codecs::jpeg::JpegDecoder};
use nokhwa::{
    Camera,
    pixel_format::RgbFormat,
    utils::{CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution},
};
use opencv::{
    core::{Mat, MatTrait, MatTraitConst, Point, Size, Vector},
    imgcodecs::{
        IMREAD_COLOR, IMREAD_GRAYSCALE, IMREAD_REDUCED_GRAYSCALE_2, IMREAD_REDUCED_GRAYSCALE_4,
        IMREAD_REDUCED_GRAYSCALE_8, IMREAD_UNCHANGED, imdecode, imread, imwrite_def,
    },
    imgproc::{
        CHAIN_APPROX_SIMPLE, INTER_CUBIC, RETR_EXTERNAL, RETR_LIST, RETR_TREE, THRESH_BINARY,
        THRESH_OTSU, bounding_rect, find_contours_def, gaussian_blur_def, resize, threshold,
    },
    objdetect::QRCodeDetector,
    prelude::QRCodeDetectorTraitConst,
};
use rqrr::PreparedImage;

use std::{
    fs::File,
    io::{Cursor, Read, Write},
    process::exit,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{net::TcpListener, thread};

use crate::{
    CAMERA_RESOLUTION_LIST, VIDEO_SOCKET,
    atomic_buf::{AtomicBuffer, AtomicBufferSplit},
};

/// Arbitrary buffer length to allow streaming/analysis to catch up with input.
const FRAME_BUFFER_SIZE: usize = 128;

type FrameBuffer = AtomicBuffer<Box<[u8]>, FRAME_BUFFER_SIZE, 5>;

fn get_camera(resolution: Option<Resolution>) -> Camera {
    // Get first valid camera idx.
    let mut camera = (0..=u32::MAX)
        .flat_map(|idx| {
            println!("Test camera idx: {idx}");
            let mut camera = Camera::new(
                CameraIndex::Index(idx),
                RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
            )
            .ok()?;
            let _valid_camera = camera
                .compatible_list_by_resolution(FrameFormat::MJPEG)
                .ok()?;
            Some(camera)
        })
        .next()
        .unwrap();

    let resolution = resolution.unwrap_or_else(|| {
        let resolutions = CAMERA_RESOLUTION_LIST.get_or_init(|| {
            let resolution_pairs: Vec<(_, _)> = camera
                .compatible_list_by_resolution(FrameFormat::MJPEG)
                .unwrap()
                .into_iter()
                .collect();
            let max_framerate = resolution_pairs
                .iter()
                .flat_map(|(_, framerate)| framerate)
                .max()
                .cloned()
                .unwrap_or(0);
            let mut resolutions: Box<[_]> = resolution_pairs
                .into_iter()
                .filter(|(_resolution, framerate)| framerate.contains(&max_framerate))
                .map(|(resolution, _framerate)| resolution)
                .collect();
            resolutions.sort_unstable();
            resolutions
        });

        *resolutions.iter().min().unwrap_or(&Resolution::default())
    });

    camera.set_resolution(resolution).unwrap();
    camera.set_frame_format(FrameFormat::MJPEG).unwrap();
    camera.open_stream().unwrap();

    camera
}

pub fn video_routine(
    qr_reads_tx: async_channel::Sender<String>,
    camera_resolution_select_rx: async_channel::Receiver<Resolution>,
) {
    let mut buffer = FrameBuffer::new();
    let AtomicBufferSplit {
        write_ptr: mut frame_write,
        mut read_ptrs,
    } = buffer.split();

    let (frame_streaming, frame_analysis) = read_ptrs.split_first_mut().unwrap();

    let flush_qr = AtomicBool::new(false);

    thread::scope(|s| {
        let camera_reader = s.spawn(|| {
            let mut resolution = None;
            'new_camera: loop {
                let mut camera = get_camera(resolution);
                println!("Camera Loaded");

                loop {
                    if let Ok(new_resolution) = camera_resolution_select_rx.try_recv() {
                        resolution = Some(new_resolution);
                        flush_qr.store(true, Ordering::Relaxed);
                        continue 'new_camera;
                    }

                    let frame = camera.frame_raw().unwrap();
                    // Discard frames whenever readers are behind.
                    let _ = frame_write.try_write(frame.as_ref());
                }
            }
        });

        let _stream_writer = s.spawn(|| {
            let mut packet = Vec::new();
            let mut frame_header_len = 0;
            let mut cur_frame_len = 0;

            'new_stream: loop {
                let listener = TcpListener::bind(VIDEO_SOCKET).unwrap();
                let (mut stream, _) = listener.accept().expect("Failed to accept connection");

                stream
                    .write_all(
                        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=--frame\r\n\r\n"
                            .as_bytes(),
                    )
                    .unwrap();

                println!("Stream Loaded");

                loop {
                    let frame = frame_streaming.read_spin();

                    // Camera frame size changed.
                    if frame.len() != cur_frame_len {
                        cur_frame_len = frame.len();
                        let frame_header = format!(
                            "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                            frame.len()
                        );
                        frame_header_len = frame_header.len();
                        packet = frame_header.as_bytes().to_vec();
                        println!("Updated Frame Size");
                    }

                    packet.extend_from_slice(&frame);
                    packet.extend_from_slice(b"\r\n");
                    if let Err(e) = stream.write_all(&packet) {
                        // Print errors for diagnostics, but loop for
                        // reconnects.
                        eprintln!("{:#?}", e);
                        continue 'new_stream;
                    }

                    // Reduce back to just the header
                    packet.truncate(frame_header_len);
                }
            }
        });

        for (scale, frame_reader) in [
            IMREAD_GRAYSCALE,
            IMREAD_REDUCED_GRAYSCALE_2,
            IMREAD_REDUCED_GRAYSCALE_4,
            IMREAD_REDUCED_GRAYSCALE_8,
        ]
        .into_iter()
        .zip(frame_analysis)
        {
            let flush_qr = &flush_qr;
            let qr_reads_tx = qr_reads_tx.clone();
            let _analysis = s.spawn(move || {
                let detector = QRCodeDetector::default().unwrap();
                let mut decoded_info = Vector::new();
                let mut points = Mat::default();

                println!("Analysis Loaded");
                loop {
                    // Whenever the resolution changes, flush QR processing.
                    // This prevents an oversized window from lagging up the
                    // pipeline with old instructions.
                    if flush_qr.load(Ordering::Relaxed) {
                        flush_qr.store(false, Ordering::Relaxed);
                        while frame_reader.try_read().is_some() {}
                    }

                    let next_frame = frame_reader.read_spin();

                    match imdecode(&&**next_frame, scale) {
                        Ok(mat_frame)
                            if mat_frame.size().is_err()
                                || mat_frame.size().is_ok_and(|size| size == Size::new(0, 0)) =>
                        {
                            eprintln!("OpenCV error! Empty image!");
                        }
                        Ok(mat_frame) => {
                            let detection = detector.detect_multi(&mat_frame, &mut points).unwrap();
                            if detection {
                                println!("Trigger: {scale}");

                                detector
                                    .decode_multi_def(&mat_frame, &points, &mut decoded_info)
                                    .unwrap();
                                for text in &decoded_info {
                                    if !text.trim().is_empty() {
                                        qr_reads_tx.try_send(text).unwrap();
                                    }
                                }

                                if decoded_info.iter().any(|text| !text.trim().is_empty()) {
                                    // Flush out remaining frames, they are probably duplicates.
                                    drop(next_frame);
                                    while frame_reader.try_read().is_some() {}
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("OpenCV read error: {e}");
                        }
                    }
                }
            });
        }

        camera_reader.join().unwrap();
        _stream_writer.join().unwrap();
    });
}
