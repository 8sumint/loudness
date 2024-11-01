use ebur128::{EbuR128, Mode};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Clone, Copy)]
struct Measurement {
    loudness: f64,
    energy: f64,
}
merde::derive! {
    impl (Deserialize, JsonSerialize) for struct Measurement { loudness, energy }
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args();
    let input = args
        .nth(1)
        .expect("usage: loudness <file/directory> [outfile]");
    let maybe_outfile = args.next();

    let data = if let Some(outfile) = &maybe_outfile {
        let outfile = Path::new(&outfile);
        if outfile.exists() {
            // load existing items
            let mut serialized = String::new();
            File::open(outfile)?
                .read_to_string(&mut serialized)
                .expect("failed to read outfile");
            let deserialized: HashMap<String, Measurement> =
                match merde::json::from_str(&serialized) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("malformed outfile: {e:?}");
                        return Ok(());
                    }
                };
            Some(RwLock::new(deserialized))
        } else {
            // create empty
            Some(RwLock::new(HashMap::new()))
        }
    } else {
        // none specified, we'll just print it so no need
        None
    };

    let path = Path::new(&input);
    if !path.exists() {
        eprintln!("Path '{}' does not exist.", path.display());
        return Ok(());
    }
    let files = if path.is_dir() {
        // multi-file
        let mut tmp = vec![];
        let contents = std::fs::read_dir(path)?;

        for entry in contents {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|p| p == "mp3") {
                tmp.push(path);
            }
        }
        tmp
    } else {
        // single file
        vec![path.to_path_buf()]
    };

    let ts = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis();

    let maybe_outfile_path = maybe_outfile.as_ref().map(Path::new);

    files.par_iter().enumerate().for_each(|(i, f)| {
        //let name = &f.to_str().unwrap().to_string();
        let name = &f.file_stem().unwrap().to_str().unwrap().to_string();
        if let Some(d) = &data {
            if d.read().unwrap().contains_key(name) {
                println!("[{}] {}: skipping", i, name);
                return;
            }
        }
        if let Ok(measurement) = measure(f) {
            if let Some(d) = &data {
                if d.read().unwrap().contains_key(name) {
                    println!("[{}] {}: skipping", i, name);
                    return;
                }
                d.write()
                    .expect("failed to acquire lock")
                    .insert(name.clone(), measurement);

                // only save sometimes
                if i % 10 == 0 {
                    save(&d.read().unwrap(), maybe_outfile_path.unwrap()).unwrap();
                }
            }
            println!(
                "[{}] {}: \t{:.2} LUFS\t{:.2} energy",
                i, name, measurement.loudness, measurement.energy
            )
        }
    });

    if let Some(d) = &data {
        // data only exists if an outfile is specified
        // this seems kinda mid
        save(&d.read().unwrap(), maybe_outfile_path.unwrap())?;
    }

    Ok(())
}

fn save(d: &HashMap<String, Measurement>, to: &Path) -> std::io::Result<()> {
    let mut file = File::create(to)?;
    let serialized = merde::json::to_string(d);
    file.write_all(serialized.as_bytes())?;
    Ok(())
}

fn measure(path: &PathBuf) -> Result<Measurement, ()> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "failed to open file '{}' for measurement: {e:?}",
                path.display()
            );
            return Err(());
        }
    };
    let file = Box::new(file);
    let mss = MediaSourceStream::new(file, Default::default());
    let hint = Hint::new();

    // Use the default options when reading and decoding.
    let format_opts: FormatOptions = Default::default();
    let metadata_opts: MetadataOptions = Default::default();
    let decoder_opts: DecoderOptions = Default::default();

    // Probe the media source stream for a format.
    let Ok(probed) =
        symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts)
    else {
        eprintln!("failed to get probe for file '{}'", path.display());
        return Err(());
    };

    // Get the format reader yielded by the probe operation.
    let mut format = probed.format;

    // Get the default track.
    let track = match format.default_track() {
        None => {
            eprintln!("file '{}' has no tracks?", path.display());
            return Err(());
        }
        Some(t) => t.clone(),
    };

    // Create a decoder for the track.
    let mut decoder =
        match symphonia::default::get_codecs().make(&track.codec_params, &decoder_opts) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "failed to create decoder for file '{}' - {e:?}",
                    path.display()
                );
                return Err(());
            }
        };

    // Store the track identifier, we'll use it to filter packets.
    let track_id = track.id;

    let channels = track.codec_params.channels.unwrap().count();

    let rate = track
        .codec_params
        .sample_rate
        .expect("has no sample rate??");

    let mut ebur128 =
        EbuR128::new(channels as u32, rate, Mode::all()).expect("Failed to create ebur128");

    let chunk_size = rate; // 1s

    //println!("{:?}", samples.samples().chunks(100).nth(5).unwrap())

    while let Ok(packet) = format.next_packet() {
        // If the packet does not belong to the selected track, skip it.
        if packet.track_id() != track_id {
            continue;
        }

        // Decode the packet into audio samples, ignoring any decode errors.
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();

                if decoded.frames() > 0 {
                    let mut sample_buffer: SampleBuffer<f32> =
                        SampleBuffer::new(decoded.frames() as u64, spec);

                    sample_buffer.copy_interleaved_ref(decoded);
                    ebur128
                        .add_frames_f32(sample_buffer.samples())
                        .expect("Failed to add frames");
                    ebur128
                        .loudness_global()
                        .expect("Failed to get global loudness");
                } else {
                    eprintln!("Empty packet encountered while loading song!");
                }
            }
            Err(Error::DecodeError(e)) => {
                eprintln!("decode error... {e:?}");
            }
            Err(Error::IoError(e)) => {
                if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof) {
                    // end of stream
                    eprintln!("end of stream during decode!");
                } else {
                    eprintln!("io error.... {e:?}");
                }
                break;
            }
            Err(e) => {
                eprintln!("other error... {e:?}");
                break;
            }
        }
    }

    let global_loudness = ebur128
        .loudness_global()
        .expect("Failed to get global loudness");

    let Some((_, energy)) = ebur128.gating_block_count_and_energy() else {
        return Err(());
    };

    // Convert dB difference to linear gain
    // let target_loudness = -14.0;
    // let gain = 10f32.powf(((target_loudness - global_loudness) / 20.0) as f32);

    Ok(Measurement {
        loudness: global_loudness,
        energy,
    })
}
