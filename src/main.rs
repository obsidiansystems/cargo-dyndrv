extern crate ffmpeg_next as ffmpeg;

fn main() {
    ffmpeg::init().unwrap();

    let version = ffmpeg::codec::version().to_be_bytes();

    println!(
        "libavcodec version {}.{}.{}, configured with {}",
        version[1],
        version[2],
        version[3],
        ffmpeg::codec::configuration()
    );
}
