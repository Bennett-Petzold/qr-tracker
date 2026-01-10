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
use rqrr::PreparedImage;

use std::{
    io::{Cursor, Write},
    sync::atomic::{AtomicBool, Ordering},
};
use std::{net::TcpListener, thread};

use crate::{
    CAMERA_RESOLUTION_LIST, VIDEO_SOCKET,
    atomic_buf::{AtomicBuffer, AtomicBufferSplit},
};

/// Arbitrary buffer length to allow streaming/analysis to catch up with input.
const FRAME_BUFFER_SIZE: usize = 128;

type FrameBuffer = AtomicBuffer<Box<[u8]>, FRAME_BUFFER_SIZE, 2>;

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
        read_ptrs: [mut frame_streaming, mut frame_analysis],
    } = buffer.split();

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

        let _analysis = s.spawn(|| {
            println!("Analysis Loaded");
            loop {
                // Whenever the resolution changes, flush QR processing.
                // This prevents an oversized window from lagging up the
                // pipeline with old instructions.
                if flush_qr.load(Ordering::Relaxed) {
                    flush_qr.store(false, Ordering::Relaxed);
                    while frame_analysis.try_read().is_some() {}
                }

                let next_frame = frame_analysis.read_spin();
                let image = DynamicImage::from_decoder(
                    JpegDecoder::new(Cursor::new(&**next_frame)).unwrap(),
                )
                .unwrap();
                let image_lum8 = image.to_luma8();
                let mut prepared = PreparedImage::prepare(image_lum8);

                for grid in prepared.detect_grids() {
                    match grid.decode() {
                        Ok((_, text)) => {
                            qr_reads_tx.try_send(text).unwrap();
                        }
                        Err(e) => eprintln!("{:#?}", e),
                    }
                }
            }
        });

        camera_reader.join().unwrap();
        _stream_writer.join().unwrap();
        _analysis.join().unwrap();
    });
}
