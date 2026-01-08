use futures::channel::mpsc::Sender;
use image::{
    codecs::jpeg::JpegDecoder, DynamicImage, ImageBuffer, ImageDecoder, ImageReader, Rgb, RgbImage,
};
use nokhwa::{
    pixel_format::RgbFormat,
    utils::{
        CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    },
    Camera,
};
use rqrr::PreparedImage;

use std::{
    io::{BufReader, Cursor, Write},
    net::UdpSocket,
    sync::{
        atomic::{AtomicU8, Ordering},
        mpsc::{self},
        OnceLock,
    },
};
use std::{net::TcpListener, thread};

use crate::{
    atomic_buf::{AtomicBuffer, AtomicBufferSplit},
    VIDEO_SOCKET,
};

/// Arbitrary buffer length to allow streaming/analysis to catch up with input.
const FRAME_BUFFER_SIZE: usize = 128;

type FrameBuffer = AtomicBuffer<Box<[u8]>, FRAME_BUFFER_SIZE, 2>;

pub static RESOLUTIONS: OnceLock<Vec<Resolution>> = OnceLock::new();

fn get_camera() -> Camera {
    let mut camera = Camera::new(
        CameraIndex::Index(0),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
    )
    .unwrap();

    let resolutions = RESOLUTIONS.get_or_init(|| {
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
        resolution_pairs
            .into_iter()
            .filter(|(_resolution, framerate)| framerate.contains(&max_framerate))
            .map(|(resolution, _framerate)| resolution)
            .collect()
    });
    let min_resolution = *resolutions.iter().min().unwrap_or(&Resolution::default());

    camera.set_resolution(min_resolution).unwrap();
    camera.set_frame_format(FrameFormat::MJPEG).unwrap();
    camera.open_stream().unwrap();

    camera
}

pub fn video_routine(mut qr_reads_tx: Sender<String>) {
    let mut camera = get_camera();

    let frame = camera.frame_raw().unwrap();
    let frame_header = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        frame.len()
    );

    drop(camera);

    let mut buffer = FrameBuffer::new();
    let AtomicBufferSplit {
        write_ptr: mut frame_write,
        read_ptrs: [mut frame_streaming, mut frame_analysis],
    } = buffer.split();

    let term_count = AtomicU8::new(0);

    let listener = TcpListener::bind(VIDEO_SOCKET).unwrap();
    let (mut stream, _) = listener.accept().expect("Failed to accept connection");

    thread::scope(|s| {
        let term_count_1 = &term_count;
        let camera_reader = s.spawn(move || {
            println!("Camera Loaded");
            // Camera isn't thread safe, needs to be recreated.
            let mut camera = get_camera();

            while term_count_1.load(Ordering::Relaxed) != 1 {
                let frame = camera.frame_raw().unwrap();
                // Discard frames whenever readers are behind.
                let _ = frame_write.try_write(frame.as_ref());
            }

            // Done with cleanup.
            term_count_1.store(0, Ordering::Relaxed);
        });

        let term_count_2 = &term_count;
        let stream_writer = s.spawn(move || {
            println!("Stream Loaded");
            stream
                .write_all(
                    "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=--frame\r\n\r\n"
                        .as_bytes(),
                )
                .unwrap();

            while term_count_2.load(Ordering::Relaxed) != 2 {
                stream.write_all(frame_header.as_bytes()).unwrap();
                let frame = frame_streaming.read_spin();
                stream.write_all(&frame).unwrap();
                stream.write_all(b"\r\n").unwrap();
            }

            // Clean up camera reading thread next.
            term_count_2.store(1, Ordering::Relaxed);
        });

        let term_count_3 = &term_count;
        let analysis = s.spawn(move || {
            println!("Analysis Loaded");
            while term_count_3.load(Ordering::Relaxed) != 2 {
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
                            // Channel got closed
                            if qr_reads_tx.try_send(text).is_err() {
                                // Clean up streaming thread next.
                                term_count_3.store(2, Ordering::Relaxed);
                                break;
                            }
                        }
                        Err(e) => eprintln!("{:#?}", e),
                    }
                }
            }
        });

        camera_reader.join().unwrap();
        stream_writer.join().unwrap();
        analysis.join().unwrap();
    });
}
