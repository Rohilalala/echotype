//! "Pause music while dictating" (macOS): Music or Spotify playing when a recording shows is
//! paused, and resumed when the recording ends. Only what Echotype paused is resumed.

use std::sync::{OnceLock, mpsc};

/// The players, by AppleScript name.
const PLAYERS: [&str; 2] = ["Music", "Spotify"];

/// The players Echotype paused and has not resumed yet.
#[derive(Default)]
struct Paused(Vec<&'static str>);

impl Paused {
    /// `pause_if_playing` pauses a player that is playing and says whether it did.
    fn pause(&mut self, mut pause_if_playing: impl FnMut(&str) -> bool) {
        for p in PLAYERS {
            if !self.0.contains(&p) && pause_if_playing(p) {
                self.0.push(p);
            }
        }
    }

    fn resume(&mut self, mut play: impl FnMut(&str)) {
        self.0.drain(..).for_each(|p| play(p));
    }
}

/// Pause (true) or resume (false) on a thread of its own: osascript takes ~50 ms a call, which a
/// recording's start must not wait for, and one queue keeps a resume from overtaking its pause.
pub fn set(pause: bool) {
    if cfg!(not(target_os = "macos")) {
        return;
    }
    static TX: OnceLock<mpsc::Sender<bool>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let mut paused = Paused::default();
            for pause in rx {
                if pause { paused.pause(pause_if_playing) } else { paused.resume(play) }
            }
        });
        tx
    });
    let _ = tx.send(pause);
}

fn osascript(script: &str) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/osascript").args(["-e", script]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `is running` first, in a script of its own: a `tell` launches a player that isn't running, and
/// a script naming one that isn't installed can make AppleScript ask where it is. The first `tell`
/// to each player asks for the Automation permission; denied, osascript fails and nothing is paused.
fn pause_if_playing(app: &str) -> bool {
    let running = format!("application \"{app}\" is running");
    osascript(&running).as_deref() == Some("true")
        && osascript(&format!("if {running} then\ntell application \"{app}\"\nif player state is playing then\npause\nreturn \"paused\"\nend if\nend tell\nend if"))
            .as_deref()
            == Some("paused")
}

/// A player quit during the dictation stays quit.
fn play(app: &str) {
    let _ = osascript(&format!("if application \"{app}\" is running then tell application \"{app}\" to play"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_echotype_paused_is_resumed_once() {
        let mut paused = Paused::default();
        paused.pause(|app| app == "Spotify"); // Spotify playing, Music stopped or not running
        let mut played = vec![];
        paused.resume(|app| played.push(app.to_string()));
        assert_eq!(played, ["Spotify"]);
        paused.resume(|_| panic!("resumed twice"));
        paused.pause(|_| false); // nothing playing at the next dictation
        paused.resume(|_| panic!("resumed a player Echotype did not pause"));
    }

    #[test]
    fn a_second_pause_before_the_resume_keeps_one_entry() {
        let mut paused = Paused::default();
        paused.pause(|_| true);
        paused.pause(|_| unreachable!("already paused players are not asked again"));
        let mut played = vec![];
        paused.resume(|app| played.push(app.to_string()));
        assert_eq!(played, ["Music", "Spotify"]);
    }
}
