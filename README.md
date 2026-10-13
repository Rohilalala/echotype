<p align="center">
  <img src=".github/assets/icon.png" width="96" alt="">
</p>

<h1 align="center">Echotype</h1>

<p align="center">
  Hold a key, speak, let go. Clean text lands wherever your cursor is.<br>
  Free, private, and it runs on the models you choose.
</p>

<p align="center">
  <a href="#set-it-up-with-your-coding-agent">Set up with your agent</a> ·
  <a href="#build-from-source">Build from source</a> ·
  <a href="#shortcuts-and-permissions">Shortcuts and permissions</a>
</p>

<p align="center">
  <img src=".github/assets/screenshot.png" width="760" alt="Echotype's speech model settings, showing local whisper models with their download size and memory needs">
</p>

## What it does

- **Dictate into any app.** Hold <kbd>⌥ Space</kbd>, talk, release. Echotype transcribes, cleans up and pastes, then puts your clipboard back.
- **Reads like you typed it.** Filler words go, "four, no wait, five" becomes "5", spoken lists become lists, "new line" starts a new line.
- **Speak Hinglish, get English.** Talk in Hindi, English or both at once; Echotype writes English.
- **Drop in a voice note.** Drag a WhatsApp `.opus`, `.m4a`, `.mp3` or `.wav` onto the window for a clean transcript.
- **Any model, both stages.** Local whisper.cpp on your machine, or any OpenAI-compatible endpoint: OpenAI, Groq, OpenRouter, Ollama, LM Studio, or your own server.
- **Knows your machine.** Echotype checks free memory before it loads or recommends a model.
- **A local API for your scripts and agents.** `POST http://127.0.0.1:7811/v1/audio/transcriptions` runs the same pipeline.

With local models, nothing leaves your computer. API keys live in the system keychain.

## Speed

15-second clip on an M5 Pro, model already loaded:

| Stage | Model | Time |
|---|---|---|
| Transcription | whisper large-v3-turbo Q5, local | 0.64 s |
| Cleanup | gemma 4 E4B via Ollama | 0.5–0.9 s |

## Set it up with your coding agent

Paste this into Claude Code, Codex, Cursor or any agent that can run commands:

```text
Set up Echotype (https://github.com/Rohilalala/echotype) on this computer.

1. Install what's missing: Rust (rustup), CMake and Node 20+. On Linux also install
   libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev libasound2-dev libxdo-dev.
2. Clone the repo, then in app/ run `npm install` and `npm run tauri build`.
3. Install the built app (macOS: copy Echotype.app from
   app/src-tauri/target/release/bundle/macos to /Applications) and open it.
4. Check how much memory is free. In Echotype's Speech model page, download the model it
   marks Recommended; pick Medium instead if I speak a language other than English.
5. Walk me through granting Microphone and Accessibility access.
6. Prove it works: run Echotype's CLI on a short recording,
   `/Applications/Echotype.app/Contents/MacOS/echotype --transcribe <file>`, and show me the text.

Don't load any local language model unless I ask. If I want cleanup, ask me which provider to use.
```

## Build from source

```sh
git clone https://github.com/Rohilalala/echotype
cd echotype/app
npm install
npm run tauri build
```

Needs Rust, CMake and Node 20+. Transcribe a file without opening the window:

```sh
echotype --transcribe note.opus
```

## Shortcuts and permissions

| Action | Default | Notes |
|---|---|---|
| Dictate | <kbd>⌥ Space</kbd> | Hold and talk, let go to paste. In toggle mode, press to start and again to finish. |
| Hands-free | Tap the dictation key twice | Keeps recording until you press the key again. |
| Cancel | <kbd>Esc</kbd> | Echotype takes Esc from other apps only while it is recording, hands-free included. |
| Paste last | <kbd>⌘ ⌃ V</kbd> | Pastes your last dictation again. Kept in memory only, so it works with history off. |
| Copy last | <kbd>⌘ ⌃ C</kbd> | Puts your last dictation on the clipboard. |

Every shortcut can be changed in Settings, and paste last and copy last can be turned off. If you upgraded from an earlier version, they start off and Settings offers them once, together with launch at login.

### The pill (macOS)

A thin bar waits at the bottom of the screen your cursor is on. Click it to dictate hands-free and click again (or ✓) to finish, ✕ cancels. Right-click it for the microphone, language, paste last, history, hide for 1 hour and settings. Turn it off under Settings → Pill, or hide it for an hour from the menu bar; it still shows while you dictate.

The pill stays out of screenshots and screen sharing unless you turn on **Show pill in screen recordings**. This is best effort: macOS honours it for screenshots and most recorders, but some screen-sharing apps may still capture it.

### Talk key (macOS)

You can also dictate with one key: **fn**, **Right Option**, **Right Command** or **Right Control**. Pick it in Settings → Shortcuts. ⌥ Space keeps working alongside it. Pressing another key while the talk key is down (fn + arrow, Right ⌘ + C) cancels the dictation, so your usual shortcuts still work.

To use **fn**, first stop macOS from acting on it: System Settings → Keyboard → **Press 🌐 key to** → **Do Nothing**. This should print `0`:

```sh
defaults read com.apple.HIToolbox AppleFnUsageType
```

### Permissions

- **Microphone**, to hear you while you dictate.
- **Accessibility**, to press ⌘V in the app you're using. Without it, Echotype copies the text and you paste it yourself.
- **Input Monitoring**, only for a talk key. Echotype watches for that one key and whether another key is pressed with it; it never records, stores or logs what you type. After granting it, relaunch Echotype. macOS ties these grants to the app's signature, so a self-built, unsigned Echotype may need them granted again after each rebuild.

## Local models

| Model | Download | Memory | Use it for |
|---|---|---|---|
| Large v3 Turbo Q5 | 547 MB | 0.8 GB | The default. Fast and accurate in 99 languages |
| Medium | 1.5 GB | 1.8 GB | Translating Hinglish or any language to English |
| Small | 488 MB | 0.9 GB | Lighter multilingual, also translates |
| Base (English) | 148 MB | 0.4 GB | Low-memory machines |

Turbo models can't translate, so with translation on Echotype switches to a multilingual model you've downloaded.

## Local API

Turn it on in Settings, copy the token, then:

```sh
curl http://127.0.0.1:7811/v1/audio/transcriptions \
  -H "Authorization: Bearer $ECHOTYPE_TOKEN" \
  -F file=@note.opus
```

It speaks the OpenAI transcription format, so OpenAI client libraries work if you point their base URL at it. It only listens on 127.0.0.1.

## Echotype Cloud

For computers that can't spare the memory, Echotype Cloud runs transcription and cleanup on a server instead. It's optional; everything above works without it.

| Plan | Price | Includes |
|---|---|---|
| Free | $0 | 2,000 words a week |
| Pro | $6 a month or $60 a year | Unlimited words, 40 audio hours a month fair use |

Pick **Echotype Cloud** as the speech provider and sign in with your email; there's no password. Audio is transcribed and dropped. The server keeps word and audio-second counts, nothing else.

### Run your own

The server is in [`server/`](server): a single Go binary in front of any OpenAI-compatible provider, with SQLite for accounts. Every setting is documented at the top of [`server/main.go`](server/main.go).

```sh
cd server
echo "STT_UPSTREAM_KEY=<your Groq key>" > .env
docker compose up -d
```

Without `SMTP_HOST` it runs in dev mode and prints sign-in codes to its log. Point the app at it with `ECHOTYPE_CLOUD_URL=http://localhost:8080/v1`.

## Credits

Built on [whisper.cpp](https://github.com/ggml-org/whisper.cpp) and [Tauri](https://tauri.app). Hotkey and paste handling borrow from [Handy](https://github.com/cjpais/Handy) (MIT).

## License

MIT
