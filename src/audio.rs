//! Audio preview: play the focused sound file in place (Space), without a
//! separate player window. One file at a time.

use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Player {
    // the stream must stay alive for the sound to keep playing
    _stream: OutputStream,
    handle: OutputStreamHandle,
    sink: Sink,
    pub path: PathBuf,
    pub duration: Option<Duration>,
}

fn decode(path: &Path) -> Result<Decoder<BufReader<File>>, String> {
    let f = File::open(path).map_err(|e| e.to_string())?;
    Decoder::new(BufReader::new(f)).map_err(|_| "미리듣기를 할 수 없는 형식입니다 (Enter: 기본 프로그램으로 열기)".to_string())
}

impl Player {
    /// Open the sound device and start playing `path` from the beginning.
    pub fn start(path: &Path) -> Result<Player, String> {
        let src = decode(path)?;
        let duration = crate::fileops::media_duration(path).or(src.total_duration());
        let (stream, handle) = OutputStream::try_default().map_err(|e| format!("소리 장치를 열 수 없습니다: {e}"))?;
        let sink = Sink::try_new(&handle).map_err(|e| e.to_string())?;
        sink.append(src);
        Ok(Player { _stream: stream, handle, sink, path: path.to_path_buf(), duration })
    }

    pub fn is_playing(&self) -> bool {
        !self.sink.is_paused() && !self.sink.empty()
    }

    pub fn finished(&self) -> bool {
        self.sink.empty()
    }

    /// Play / pause; after the end it starts over.
    pub fn toggle(&mut self) -> Result<(), String> {
        if self.sink.empty() {
            let src = decode(&self.path)?;
            self.sink = Sink::try_new(&self.handle).map_err(|e| e.to_string())?;
            self.sink.append(src);
        } else if self.sink.is_paused() {
            self.sink.play();
        } else {
            self.sink.pause();
        }
        Ok(())
    }

    pub fn pos(&self) -> Duration {
        match (self.finished(), self.duration) {
            (true, Some(d)) => d,
            _ => self.sink.get_pos(),
        }
    }

    pub fn seek(&mut self, to: Duration) {
        if self.sink.empty() {
            // ended: load it again, paused, so the seek has something to move
            if let Ok(src) = decode(&self.path) {
                if let Ok(s) = Sink::try_new(&self.handle) {
                    s.pause();
                    s.append(src);
                    self.sink = s;
                }
            }
        }
        let _ = self.sink.try_seek(to);
    }
}

pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}
