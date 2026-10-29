//! Fully automatic native render probe. All PCM is masked to zero before native copy.
use oink_audio::{dsp, playback, sink};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
fn main() -> Result<(), String> {
    if std::env::var_os("ACROSS_DEVICE_TEST_HELD").is_none() {
        return Err("set ACROSS_DEVICE_TEST_HELD only while this machine is reserved for the probe".into());
    }
    let mode = std::env::var("OINK_PROBE_MODE")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0);
    let device = playback::outputs()?
        .into_iter()
        .find(|d| d.is_default)
        .ok_or("no default output")?;
    let mut output = sink::Sink::start_silent_test(
        &device.id,
        &playback::PlaybackOptions {
            trace: true,
            ..Default::default()
        },
    )?;
    output.set_output_quality_counters(Default::default());
    let config = dsp::Config::low_latency_opus();
    let mut encoder = dsp::Encoder::new_with_config(config).map_err(|e| e.to_string())?;
    let samples: Vec<_> = (0..240)
        .flat_map(|i| {
            let s = (i as f32 * 0.1).sin() * 0.02;
            [s, -s]
        })
        .collect();
    let encoded = encoder.encode(&samples).map_err(|e| e.to_string())?;
    let origin = Instant::now();
    let mut sources = Vec::new();
    for _ in 0..16 {
        let source = output.source(config)?;
        source.set_dropout_counters(Default::default());
        let sequence = AtomicU64::new(0);
        let mut payload = [0; dsp::MAX_PACKET_BYTES];
        payload[..encoded.len()].copy_from_slice(&encoded);
        let len = encoded.len();
        source.set_packet_reader(Arc::new(move |budget| {
            if *budget == 0 {
                return (true, None);
            }
            *budget -= 1;
            let seq = sequence.load(Ordering::Relaxed);
            let arrived = origin + Duration::from_millis(seq * 5);
            if arrived > Instant::now() {
                return (true, None);
            }
            sequence.store(seq + 1, Ordering::Relaxed);
            let burst = (128..132).contains(&(seq % 256));
            if mode == 1 && burst {
                return (true, None);
            }
            let (payload, len) = if mode == 2 && burst {
                let mut bad = [0; dsp::MAX_PACKET_BYTES];
                bad[..2].copy_from_slice(&[0x8b, 0x41]);
                (bad, 2)
            } else {
                (payload, len)
            };
            (
                true,
                Some(sink::Packet {
                    burst: 1,
                    sequence: seq,
                    payload,
                    len,
                    arrived,
                    network_read: None,
                    source_emit: Some(seq * 5_000_000),
                }),
            )
        }));
        sources.push(source);
    }
    while origin.elapsed() < Duration::from_secs(6) {
        std::thread::sleep(Duration::from_millis(10));
        for source in &mut sources {
            source.poll();
        }
        if let Some(error) = output.error() {
            return Err(error);
        }
    }
    eprintln!(
        "SILENT_NATIVE mode={mode} info={:?} metrics={:?}",
        output.info(),
        output.take_metrics()
    );
    for (index, source) in sources.iter().enumerate() {
        eprintln!("SOURCE {index} {:?}", source.stats());
    }
    drop(sources);
    drop(output);
    Ok(())
}
