use auricle_lib::core::stream_player::{get_stream_url, StreamingAudioSource};
use rodio::Source;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[test]
#[ignore = "requires network access and AURICLE_TEST_VIDEO_ID"]
fn uncached_stream_produces_audio_promptly() {
    let video_id = std::env::var("AURICLE_TEST_VIDEO_ID")
        .expect("set AURICLE_TEST_VIDEO_ID to a public video id");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = (|| -> Result<(), String> {
            let url = get_stream_url(&video_id)?;
            eprintln!("[stream-test] stage=resolved elapsed_ms={}", started.elapsed().as_millis());
            let mut source = StreamingAudioSource::from_url(&url)?;
            eprintln!("[stream-test] stage=opened elapsed_ms={}", started.elapsed().as_millis());
            source.next().ok_or("stream ended before the first sample")?;
            eprintln!("[stream-test] stage=first-sample elapsed_ms={}", started.elapsed().as_millis());
            let sample_count = source.sample_rate() as usize * source.channels() as usize * 2;
            if source.take(sample_count).count() != sample_count {
                return Err("stream ended before two seconds of audio".into());
            }
            eprintln!("[stream-test] stage=two-seconds elapsed_ms={}", started.elapsed().as_millis());
            Ok(())
        })();
        let _ = sender.send(result);
    });
    receiver.recv_timeout(Duration::from_secs(25))
        .expect("uncached stream startup exceeded 25 seconds")
        .expect("uncached stream failed");
}