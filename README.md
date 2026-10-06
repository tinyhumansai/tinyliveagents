# tinyliveagents

One standard Rust API for **live voice agents**: realtime, two-way speech
conversations with a model that can call functions mid-conversation.

Gemini Live, ElevenLabs Agents and Sarvam AI each speak a different WebSocket
protocol, with different audio framing, transcript semantics, tool-call shapes
and close codes. `tinyliveagents` puts one vocabulary in front of all of them:

```rust
use tinyliveagents::{LiveConfig, LiveEvent, LiveProvider, ToolDeclaration, ToolResult};
use tinyliveagents::sarvam::SarvamCascade;

let provider = SarvamCascade::new(api_key);          // or GeminiLive, GeminiRelay, ElevenLabsConvai
let mut session = provider
    .connect(
        LiveConfig::new()
            .with_system_instruction("You are a concise voice assistant.")
            .with_tool(ToolDeclaration::new("get_time", "Current time", schema)),
    )
    .await?;
let sender = session.sender();                       // feed microphone PCM16 @ 16 kHz here
while let Some(event) = session.recv().await {
    match event {
        LiveEvent::Audio(pcm) => play(pcm),          // agent speech, rate in Ready
        LiveEvent::ToolCall(call) => sender.send_tool_result(ToolResult::ok(&call, run(&call))).await?,
        LiveEvent::Interrupted => flush_playback(),  // user barged in
        LiveEvent::Closed(_) => break,
        _ => {}
    }
}
```

## Providers

| Provider | Type | Feature | How it connects |
| --- | --- | --- | --- |
| Gemini Live, direct | `gemini::GeminiLive` | `gemini` | Google API key; native audio, 16 kHz in / 24 kHz out |
| Gemini Live, relayed | `gemini::GeminiRelay` | `gemini` | A relay ticket URL minted by the host (e.g. the TinyHumans backend); build the mint request with `gemini::ticket_request` |
| ElevenLabs Agents | `elevenlabs::ElevenLabsConvai` | `elevenlabs` | A signed URL minted by a backend, or an agent id + API key |
| Sarvam AI | `sarvam::SarvamCascade` | `sarvam` | API key; chains streaming STT → chat completions (tools) → streaming TTS |

All four emit the same events: `Ready`, `Audio`, `InputTranscript` /
`OutputTranscript` (partial text replaced until `is_final`), `ToolCall`,
`ToolCallCancelled`, `Interrupted`, `TurnComplete`, `ResumptionHandle`,
`GoAway`, `Error`, and `Closed`. Close codes and handshake refusals map onto one
`Error` enum (`Unauthorized`, `InsufficientCredits`, `RateLimited`, `Timeout`,
...).

## What this crate does not do

It standardizes provider APIs and nothing more:

- it never **executes tools**: a `ToolCall` goes to the host, which answers
  with a `ToolResult`;
- it never **stores or looks up credentials** and never **mints** relay
  tickets or signed URLs: providers are constructed with what they need;
- it applies **no policy**: approvals, tool scoping and transcript persistence
  belong to the host or to an agent harness on top (OpenHuman runs sessions
  through `tinyagents-live`).

## Layout

```text
crates/
├── tinyliveagents/            # the library
│   └── src/
│       ├── types/             # LiveConfig, ClientCommand, LiveEvent, ToolCall, ...
│       ├── session/           # LiveSession, LiveSender, LiveEvents
│       ├── provider/          # the LiveProvider trait
│       ├── transport/         # WebSocket connect + the generic codec driver
│       ├── audio/             # PCM16 conversion and resampling
│       ├── gemini/            # setup, schema cleaning, wire codec, providers
│       ├── elevenlabs/        # wire codec, provider
│       └── sarvam/            # stt, chat, tts, chunker, cascade, provider
└── tinyliveagents-examples/   # runnable examples + live (network) tests
```

Each module keeps its tests in a sibling `<module>_tests.rs` (`mod_tests.rs`
beside a `mod.rs`). Provider tests run against in-process mock WebSocket and
HTTP servers, so `cargo test` never touches the network.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
.github/scripts/check-file-coverage.sh 90 coverage.json   # 90% per file
```

### Live tests and examples

`tinyliveagents-examples` talks to real providers. Its tests are `#[ignore]`d
and skip when their key is unset:

```sh
SARVAM_API_KEY=... cargo test -p tinyliveagents-examples --test live -- --ignored --nocapture
SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example sarvam_tool_call -- "" reply.wav
GEMINI_API_KEY=... SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example gemini_direct
TINYHUMANS_API_KEY=... SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example gemini_relay
TINYHUMANS_API_KEY=... SARVAM_API_KEY=... cargo run -p tinyliveagents-examples --example elevenlabs_relay
```

The spoken question comes from `LIVE_TEST_WAV` (16 kHz PCM16 mono) or is
synthesized with Sarvam's REST TTS. See [`.env.example`](.env.example).

## Releasing

Run the **Release** workflow (`workflow_dispatch`) with a `patch` / `minor` /
`major` bump. It validates, bumps the workspace version, tags `vX.Y.Z`, and
creates a GitHub release. Consumers pin the tag as a git dependency.

## Documentation

- [`AGENTS.md`](AGENTS.md): conventions for humans and coding agents.
- [`docs/specs/live-session.md`](docs/specs/live-session.md): the standard
  session contract and each provider's mapping onto it.
- [`ROADMAP.md`](ROADMAP.md): what is shipped and what is next.

## License

GPL-3.0-only. See [`LICENSE`](LICENSE).
