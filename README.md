# ElevenDesk 2

A desktop client for the ElevenLabs API, rewritten in Rust. Not affiliated with or endorsed by ElevenLabs.

## Why this project exists

I created ElevenDesk to put the parts of ElevenLabs I actually use into a native desktop app with real controls, a sensible tab order and clear status messages, instead of doing everything through the website.

## Why ElevenDesk 2

The original ElevenDesk was written in Python with wxPython, and Python served me well for many years. ElevenDesk 2 is the same app rebuilt in Rust on top of wxDragon (the Rust bindings for wxWidgets), mainly because:

- **Rust is faster.** Network work, audio decoding and conversion run on their own threads, and the interface stays out of their way.
- **It is easier to distribute.** A Rust build is a native executable. There is no Python runtime to install, no virtual environment and no bundler step to freeze it.
- **It is lightweight.** No interpreter, and the interface is still made of native operating system controls.

Everything the original did is still here. A few rough edges were fixed along the way, and the model list now comes from ElevenLabs instead of being hard-coded.

## Requirements

- An ElevenLabs API key
- Windows 10 or 11. macOS and Linux have not been tested.
- A working audio output device

## Installation

There are no prebuilt releases yet, so build it from source (next section) and run the executable.

On first launch ElevenDesk 2 opens Preferences so you can enter your API key and choose an output folder.

## Building and running from source

You need:

- [Rust](https://rustup.rs/) 1.88 or newer
- On Windows, the Visual Studio C++ build tools (MSVC)
- CMake and a C++ compiler on your path, because wxDragon builds wxWidgets as part of the first build

Then:

```shell
git clone https://github.com/ivansoto0/elevendesk2
cd elevendesk2
cargo run --release
```

The first build compiles wxWidgets, so it takes a while. Later builds only recompile what changed. A plain `cargo run` gives you a debug build, which builds faster but runs slower.

To build without running, use `cargo build --release`. The executable ends up in `target/release/`. On Windows it is a single `.exe` with its manifest embedded.

Run the tests with:

```shell
cargo test
```

They cover model parsing, error handling, audio encoding and decoding, and the API client against a local mock server. They need no network and no API key.

### Code style

The project is indented with tabs and linted with pedantic clippy. CI enforces both, and you can run the same checks locally:

```shell
cargo +nightly fmt --check
cargo clippy --all-targets -- -D warnings
```

Formatting needs the nightly toolchain only because `rustfmt.toml` uses nightly import-grouping options.

The release profile is tuned for speed and a single self-contained executable: opt-level 3, fat LTO, one codegen unit, `panic = "abort"` and stripped symbols.

## Settings and API key

Settings and the API key live in one config folder, chosen by the [homeport](https://crates.io/crates/homeport) crate:

- Installed copy (an Inno Setup `unins*.exe` next to the executable): `%APPDATA%\ElevenDesk2`
- Portable copy (anything else): the folder the executable is in
- Set `ELEVENDESK2_CONFIG_DIR` to use a different folder, for example to run a second copy with separate settings

The folder holds `settings.json`, which includes your API key. The key is stored as plain text and is never written to logs, so anyone who can read that folder can read the key. Keep it private.

For development, the `ELEVENLABS_API_KEY` environment variable is used if no key has been saved.

## Keyboard shortcuts

Everything can be reached with Tab and the menus. The same list is in the app under Help > Keyboard Shortcuts.

- `Ctrl+Enter` — run the current tab's main action (generate, transcribe, convert, start dubbing, design a voice)
- `Ctrl+S` — save the last output
- `Ctrl+O` — open an audio file for the current tab
- `Ctrl+P` — play, pause or resume
- `Ctrl+.` — stop playback, or cancel a running job
- `Ctrl+1` to `Ctrl+6` — switch tab
- `F6` — move to the current tab's Status field
- `Ctrl+Shift+L` — repeat the last announcement
- `Ctrl+R` — reload voices and models
- `Ctrl+,` — Preferences
- `Enter` / `Delete` on the dubbing project list — open / delete the selected project

## Features

- Text to speech with voice, model, output format and voice setting controls, playing the audio while it is still being generated
- Speech to text transcription
- Speech to speech, from a file or straight from the microphone
- Sound effect generation with looping and prompt-influence controls
- Dubbing for audio and video sources, using the dubbing project API
- Voice design and cloning, including recursive folder import for cloning
- Voice library with search, preview, settings, rename and delete
- Generation history with playback, download and delete
- Pronunciation dictionary listing

## Models

Models are fetched from ElevenLabs when the app starts and are kept as plain text IDs. What each workflow offers follows the capability fields the API returns, so newer models such as Eleven v4 appear on their own if your account has them, and older ones stay available. If a model you saved is no longer offered, you are told and a replacement is chosen. If the list cannot be loaded, the original four models are shown and labelled as a built-in list.

## Contributing

Contributions are welcome, from bug reports to pull requests. For anything bigger than a small fix, open an issue first so we can agree on the approach.

AI-assisted contributions are welcome too. This application has been largely vibe coded, so I would be a hypocrite to say otherwise. The only thing I ask is that you check the code before you send it: read it, build it, run it and try the change by hand. Don't send something you haven't looked at yourself.

Before you send a pull request, run the same checks CI runs. All three must pass:

```shell
cargo +nightly fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- Indent with tabs. `cargo +nightly fmt` does this for you.
- Keep ElevenLabs calls inside `src/api`, and keep network and audio work off the GUI thread.
- Add a test for logic that can run without the interface.
- Never include an API key in a commit, an issue or a log.

## License

MIT. See [LICENSE](LICENSE).
